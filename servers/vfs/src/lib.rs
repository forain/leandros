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

#![no_std]

use ipc::{Message, port};
use spin::Mutex;

extern crate alloc;
extern crate mm;

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
pub const VFS_TIMERFD_CREATE:  u64 = 0x25; // timerfd_create(clockid) → fd
pub const VFS_TIMERFD_SETTIME: u64 = 0x26; // timerfd_settime(fd, flags, new_ns, interval_ns)
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
fn poll_reply(revents: u32, seq: u64) -> Message {
    let mut m = make_reply(revents as i64);
    m.data[8..16].copy_from_slice(&seq.to_le_bytes());
    m
}

// ── IPC Call helper ──────────────────────────────────────────────────────────

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
            match port::create(caller) {
                Some(p) => {
                    sched::set_current_reply_port(p);
                    p
                }
                // A zeroed Message::empty() decodes as val_reply(0) — a
                // phantom success (e.g. an empty file) rather than a visible
                // error. Callers like handle_open's MountedFile path only
                // check `< 0`, so this must be a real negative errno.
                None    => return err_reply(-12), // ENOMEM
            }
        }
    };

    msg.reply_port = reply_port;
    if port::send(port_id, msg).is_err() {
        return err_reply(-12); // ENOMEM
    }

    let caller = sched::current_pid();
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

// Every entry is stored inline, so these bounds are a straight static-memory
// trade: MAX_TMP_FILES * (MAX_TMP_SIZE + MAX_TMP_PATH) ≈ 4.2 MB of BSS.
// The previous 32 / 4 KiB / 64 was far too tight for real userland — a
// coreutils run in /tmp exhausts 32 slots quickly and then reports ENOSPC
// from creat/mkdir, and 64 bytes of path cannot hold a couple of nested
// directories. Raise before assuming a tmpfs failure is a logic bug.
const MAX_TMP_FILES: usize = 128;
const MAX_TMP_SIZE:  usize = 32768;
const MAX_TMP_PATH:  usize = 128;

struct TmpFileEntry {
    path:     [u8; MAX_TMP_PATH],
    path_len: usize,
    data:     [u8; MAX_TMP_SIZE],
    len:      usize,
    in_use:   bool,
    is_dir:   bool,
    /// True for a tmpfs entry created via mknod(S_IFIFO) — reported as
    /// S_IFIFO/DT_FIFO by stat/fstat/getdents64. Does NOT get real FIFO
    /// read/write semantics; see the scope note on `handle_mknod`.
    is_fifo:  bool,
    /// True for a tmpfs entry created via symlink(2). `data[..len]` holds the
    /// link *target* verbatim — it is never normalised or resolved at creation
    /// time, so a dangling or relative target is stored exactly as given and
    /// only interpreted during lookup (see `tmp_resolve_links`).
    is_link:  bool,
    /// True for an AF_UNIX bound-socket node (created by `unix_bind_node` when
    /// a process `bind()`s a pathname socket). Reported as S_IFSOCK by
    /// stat/fstat/getdents64. `sock_id` links it back to the net server's
    /// listener so `connect` can resolve the path to the right socket.
    is_sock:  bool,
    sock_id:  u64,
    /// Hard-link indirection. `usize::MAX` means "this entry owns its own
    /// bytes"; anything else is the pool index of the entry that does.
    ///
    /// A tmpfs "inode" is therefore the data-owning slot, and every name
    /// pointing at it is a separate slot whose `link_to` names the owner.
    /// `st_ino` and every read/write/truncate funnel through `tmp_owner()`, so
    /// two hard links genuinely share one file rather than two copies of it.
    /// Directories are never hard-linked (link(2) returns EPERM for them), so
    /// `link_to` is always `usize::MAX` on an `is_dir` slot.
    link_to:  usize,
    mode:     u32, // permission bits (rwxrwxrwx), set at creation from umask
    uid:      u32, // owner, set at creation from the creating task's euid
    gid:      u32, // owner group, set at creation from the creating task's egid
    /// Synthetic /proc snapshot parked in the pool under a fake "/tmp/.proc_N"
    /// path. Owned by the fd rather than by a name: invisible to lookup and to
    /// getdents64, and freed on close (see `tmp_release_ephemeral`).
    ephemeral: bool,
    /// Per-inode extended-attribute arena, shared wire format with f2fs (see
    /// the `xattr` crate). Belongs to the data-owning slot: every xattr op maps
    /// through `tmp_owner()` first, so hard links share one set of attributes
    /// (and one stored POSIX ACL) exactly as they share their bytes.
    xattr: [u8; xattr::TMP_XATTR_ARENA],
}

impl TmpFileEntry {
    const fn empty() -> Self {
        Self { path: [0u8; MAX_TMP_PATH], path_len: 0,
               data: [0u8; MAX_TMP_SIZE], len: 0,
               in_use: false, is_dir: false, is_fifo: false, is_link: false,
               is_sock: false, sock_id: 0,
               link_to: usize::MAX,
               mode: 0, uid: 0, gid: 0, ephemeral: false,
               xattr: [0u8; xattr::TMP_XATTR_ARENA] }
    }
}

static TMP_FILES: Mutex<[TmpFileEntry; MAX_TMP_FILES]> =
    Mutex::new([const { TmpFileEntry::empty() }; MAX_TMP_FILES]);

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
// A file that is never memfd'd and never `MAP_SHARED`-mapped keeps its inline
// `TmpFileEntry.data` path byte-for-byte, so vfstest/coreutils tmpfs I/O is
// unchanged. The VMO branch in read/write/ftruncate activates only when
// `TMP_VMOS[owner].is_some()`.
struct TmpVmo {
    /// Physical frame for each 4 KiB page index; non-sparse (every index in
    /// `0..pages.len()` is a real buddy frame). Capacity (frame count) is
    /// decoupled from `len`: a mapping larger than the file grows `pages`
    /// without moving EOF.
    pages: alloc::vec::Vec<usize>,
    /// Logical file size in bytes (EOF). Mirrored into `TmpFileEntry.len` so
    /// fstat/lseek/poll — which read `entry.len` — stay correct unchanged.
    len:  usize,
    /// `F_SEAL_*` bits. Only `F_SEAL_SHRINK` is enforced (in ftruncate).
    seals: u32,
    /// Seals are only permitted on memfd inodes; also gates `F_GET_SEALS`.
    is_memfd: bool,
}

static TMP_VMOS: Mutex<[Option<TmpVmo>; MAX_TMP_FILES]> =
    Mutex::new([const { None }; MAX_TMP_FILES]);

// Linux memfd/fcntl seal bits (values fixed by the ABI).
const F_ADD_SEALS:   usize = 1033;
const F_GET_SEALS:   usize = 1034;
const F_SEAL_SHRINK: u32   = 0x0002;

/// Allocate one zeroed 4 KiB buddy frame for a VMO. Zeroing touches HHDM
/// (kernel) memory only, never user memory.
fn vmo_alloc_zeroed_frame() -> Option<usize> {
    let phys = mm::buddy::alloc(0)?;
    unsafe { (mm::phys_to_virt(phys) as *mut u8).write_bytes(0, 4096); }
    Some(phys)
}

/// Copy `n` bytes out of the VMO frames starting at logical offset `off` into
/// kernel/user `dst`. Walks page by page (an unaligned `off` may cross one
/// frame boundary). Every touched page index is guaranteed present.
unsafe fn vmo_copy_out(vmo: &TmpVmo, off: usize, dst: *mut u8, n: usize) {
    let mut done = 0usize;
    while done < n {
        let pos  = off + done;
        let page = pos / 4096;
        let poff = pos % 4096;
        let cnt  = (4096 - poff).min(n - done);
        let src  = (mm::phys_to_virt(vmo.pages[page]) + poff) as *const u8;
        core::ptr::copy_nonoverlapping(src, dst.add(done), cnt);
        done += cnt;
    }
}

/// Copy `n` bytes from `src` into the VMO frames at logical offset `off`. The
/// caller must have grown `pages` to cover `off + n` first.
unsafe fn vmo_copy_in(vmo: &mut TmpVmo, off: usize, src: *const u8, n: usize) {
    let mut done = 0usize;
    while done < n {
        let pos  = off + done;
        let page = pos / 4096;
        let poff = pos % 4096;
        let cnt  = (4096 - poff).min(n - done);
        let dst  = (mm::phys_to_virt(vmo.pages[page]) + poff) as *mut u8;
        core::ptr::copy_nonoverlapping(src.add(done), dst, cnt);
        done += cnt;
    }
}

/// Zero bytes `[from, to)` of the VMO frames (HHDM only). Used to clear the
/// tail of the last previously-existing page on ftruncate-grow.
fn vmo_zero_range(vmo: &mut TmpVmo, from: usize, to: usize) {
    let mut pos = from;
    while pos < to {
        let page = pos / 4096;
        if page >= vmo.pages.len() { break; }
        let poff = pos % 4096;
        let cnt  = (4096 - poff).min(to - pos);
        unsafe { ((mm::phys_to_virt(vmo.pages[page]) + poff) as *mut u8).write_bytes(0, cnt); }
        pos += cnt;
    }
}

/// Release a VMO slot's implicit references and clear it. Called from every
/// inode-free site (`tmp_drop_name`, `tmp_release_ephemeral`) with `TMP_FILES`
/// already held — the lock order is TMP_FILES → TMP_VMOS. A frame still held by
/// a live mapping survives (its `pageref` > 1); the last holder frees it.
fn vmo_free_slot(owner: usize) {
    if let Some(vmo) = TMP_VMOS.lock()[owner].take() {
        for phys in vmo.pages {
            if phys != 0 { mm::pageref::unref_or_free(phys, 0); }
        }
    }
}

/// Resolve `(pid, fd)` to the owning tmpfs slot index, or `None` if the fd is
/// not an open `TmpFile`.
fn tmpfile_owner_of(pid: u32, fd: usize) -> Option<usize> {
    let mut tbls = FD_TABLES.lock();
    let tbl = find_tbl(pid, &mut *tbls)?;
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return None; }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => Some(idx),
        _ => None,
    }
}

/// Mark the tmpfs inode behind `fd` as a memfd: create an empty seal-capable
/// VMO on its owner slot. Called by `sys_memfd_create` right after open.
pub fn mark_memfd(pid: u32, fd: usize) {
    let idx = match tmpfile_owner_of(pid, fd) { Some(i) => i, None => return };
    let mut vmos = TMP_VMOS.lock();
    match vmos[idx].as_mut() {
        Some(vmo) => vmo.is_memfd = true,
        None => vmos[idx] = Some(TmpVmo {
            pages: alloc::vec::Vec::new(), len: 0, seals: 0, is_memfd: true,
        }),
    }
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
    -> Option<alloc::vec::Vec<usize>>
{
    if len == 0 || off % 4096 != 0 { return None; }
    let idx = tmpfile_owner_of(pid, fd)?;
    let first = off / 4096;
    let n = (len + 4095) / 4096;
    let need_pages = first.checked_add(n)?;

    let mut tmp = TMP_FILES.lock();
    let mut vmos = TMP_VMOS.lock();

    // Promote a plain tmpfs file on first MAP_SHARED: migrate inline bytes.
    if vmos[idx].is_none() {
        let cur_len = tmp[idx].len;
        let cur_pages = (cur_len + 4095) / 4096;
        let mut pages = alloc::vec::Vec::new();
        for p in 0..cur_pages {
            let phys = match vmo_alloc_zeroed_frame() {
                Some(f) => f,
                None => { for &f in &pages { mm::buddy::free(f, 0); } return None; }
            };
            let start = p * 4096;
            let cnt = (cur_len - start).min(4096);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    tmp[idx].data.as_ptr().add(start),
                    mm::phys_to_virt(phys) as *mut u8, cnt);
            }
            pages.push(phys);
        }
        vmos[idx] = Some(TmpVmo { pages, len: cur_len, seals: 0, is_memfd: false });
    }

    let vmo = vmos[idx].as_mut().unwrap();
    // Grow frame capacity to cover the mapped range. This does NOT move EOF
    // (`vmo.len`) — a mapping past end-of-file gets zero-filled frames.
    while vmo.pages.len() < need_pages {
        let phys = vmo_alloc_zeroed_frame()?; // partial growth is harmless (VMO owns them)
        vmo.pages.push(phys);
    }

    // Pin and collect the mapped range's frames (inc under the lock).
    let mut out = alloc::vec::Vec::with_capacity(n);
    for p in first..need_pages {
        let phys = vmo.pages[p];
        mm::pageref::inc(phys);
        out.push(phys);
    }
    Some(out)
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
    /// One end of a pipe.
    Pipe { ring: usize, is_write: bool },
    /// Writable entry in the TmpFiles pool (idx into TMP_FILES).
    TmpFile { idx: usize, pos: usize, writable: bool },
    /// eventfd: counter value; read returns counter as u64, write adds to it.
    EventFd { slot: usize },
    /// timerfd: index into TIMERFD_POOL.
    TimerFd { slot: usize },
    /// /dev/urandom — reads return LFSR pseudo-random bytes.
    DevUrandom,
    /// /dev/stdin|stdout|stderr — proxy to fd 0/1/2 of the owning process.
    DevStdio { target_fd: usize },
    /// /dev/fb0 — linear framebuffer.
    DevFb { pos: usize },
    /// Dynamically registered device proxy.
    DynamicDevice { port: u32, dev_id: u32 },
    /// File or directory on a mounted filesystem (F2FS, etc.).
    MountedFile { port: u32, file_id: u32 },
}

/// True if `fd` was opened with (or fcntl'd to) O_NONBLOCK. The kernel's
/// sys_read EAGAIN retry loop consults this: blocking fds yield-and-retry,
/// non-blocking fds must surface EAGAIN to the caller (POSIX). Without this
/// check a non-blocking read of an empty device (e.g. MAME polling
/// /dev/input/event0) never returns and the caller spins in-kernel forever.
pub fn fd_nonblock(pid: u32, fd: usize) -> bool {
    const O_NONBLOCK: u32 = 0o4000;
    let pid = sched::tgid_of(pid); // fd tables are per-process
    let mut tbls = FD_TABLES.lock();
    if let Some(tbl) = find_tbl(pid, &mut *tbls) {
        if fd < MAX_FDS && tbl.fds[fd].in_use {
            return tbl.fds[fd].flags & O_NONBLOCK != 0;
        }
    }
    false
}

/// True when `fd` is a `/dev/stdin|stdout|stderr` proxy whose target is the
/// raw console (untracked fd 0-2, or itself another console proxy). The
/// kernel's console fast paths treat such fds exactly like bare 0/1/2 —
/// without this, a dup'd stdio fd dup2'd back onto 0/1/2 (command_fds'
/// identity mappings) would recurse inside the VFS instead of reaching the
/// serial console.
pub fn fd_is_console_stdio(pid: u32, fd: usize) -> bool {
    let pid = sched::tgid_of(pid);
    if fd >= MAX_FDS { return false; }
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return false };
    let mut cur = fd;
    // Follow at most a few proxy hops (cycles collapse to "console").
    for _ in 0..4 {
        if !tbl.fds[cur].in_use { return cur <= 2; }
        match tbl.fds[cur].kind {
            VnodeKind::DevStdio { target_fd } => {
                if target_fd == cur { return true; }
                cur = target_fd;
            }
            _ => return false,
        }
    }
    true
}

/// Transfer ownership of a mounted-file fd out of `pid`'s FD table.
///
/// Frees the fd slot WITHOUT proxying VFS_CLOSE to the mount, so the
/// filesystem-side open file stays live; the caller (the kernel's
/// demand-paged exec path) becomes responsible for closing it via the
/// mount's port when the last reference goes away.  Returns the mount port
/// and file id, or None if `fd` is not a MountedFile.
pub fn steal_mounted_file(pid: u32, fd: usize) -> Option<(u32, u32)> {
    let mut tbls = FD_TABLES.lock();
    let tbl = find_tbl(pid, &mut *tbls)?;
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return None; }
    if let VnodeKind::MountedFile { port, file_id } = tbl.fds[fd].kind {
        tbl.fds[fd] = FdEntry::empty();
        Some((port, file_id))
    } else {
        None
    }
}

/// Identify the kind of a vnode from a process's FD table.
pub fn vfs_get_node_kind(pid: u32, fd: usize) -> Option<VnodeKind> {
    let mut tbls = FD_TABLES.lock();
    if let Some(tbl) = find_tbl(pid, &mut *tbls) {
        if fd < MAX_FDS {
            if tbl.fds[fd].in_use {
                return Some(tbl.fds[fd].kind);
            }
        }
    }
    None
}

// ── Dynamic Device Registry ───────────────────────────────────────────────────

const MAX_DYNAMIC_DEVICES: usize = 16;

#[derive(Clone, Copy)]
pub struct DynamicDeviceEntry {
    pub path: &'static str,
    pub port: u32,
    pub dev_id: u32,
    pub in_use: bool,
}

impl DynamicDeviceEntry {
    const fn empty() -> Self {
        Self { path: "", port: 0, dev_id: 0, in_use: false }
    }
}

static DYNAMIC_DEVICES: Mutex<[DynamicDeviceEntry; MAX_DYNAMIC_DEVICES]> =
    Mutex::new([const { DynamicDeviceEntry::empty() }; MAX_DYNAMIC_DEVICES]);

/// Register a dynamic device path to be proxied to a specific IPC port.
pub fn register_device(path: &'static str, port: u32, dev_id: u32) {
    let mut devices = DYNAMIC_DEVICES.lock();
    if let Some(slot) = devices.iter_mut().find(|d| !d.in_use) {
        *slot = DynamicDeviceEntry { path, port, dev_id, in_use: true };
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
}

impl MountEntry {
    const fn empty() -> Self { Self { prefix: "", port: 0, in_use: false, device: "", fstype: "" } }
}

static MOUNTS: Mutex<[MountEntry; MAX_MOUNTS]> =
    Mutex::new([const { MountEntry::empty() }; MAX_MOUNTS]);

/// Register a mounted filesystem at `prefix` (e.g. "/mnt") to an IPC `port`.
pub fn register_mount(prefix: &'static str, port: u32, device: &'static str, fstype: &'static str) {
    let mut m = MOUNTS.lock();
    if let Some(slot) = m.iter_mut().find(|e| !e.in_use) {
        *slot = MountEntry { prefix, port, in_use: true, device, fstype };
    }
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
        true
    } else {
        false
    }
}

/// Longest-prefix match: if `path` falls under any registered mount, return its port.
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
const PIPE_RING_SIZE: usize = 16384;
const MAX_PIPES:      usize = 16;

struct PipeRing {
    buf:         [u8; PIPE_RING_SIZE],
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
}

impl PipeRing {
    const fn new() -> Self {
        Self {
            buf: [0u8; PIPE_RING_SIZE],
            read_pos: 0, write_pos: 0, count: 0,
            readers: 0, writers: 0,
            seq: 0,
        }
    }

    fn put(&mut self, b: u8) -> bool {
        if self.count >= PIPE_RING_SIZE { return false; }
        self.buf[self.write_pos] = b;
        self.write_pos = (self.write_pos + 1) % PIPE_RING_SIZE;
        self.count += 1;
        true
    }

    fn get(&mut self) -> Option<u8> {
        if self.count == 0 { return None; }
        let b = self.buf[self.read_pos];
        self.read_pos = (self.read_pos + 1) % PIPE_RING_SIZE;
        self.count -= 1;
        Some(b)
    }
}

static PIPE_RINGS: Mutex<[PipeRing; MAX_PIPES]> =
    Mutex::new([const { PipeRing::new() }; MAX_PIPES]);

/// Bump the reader/writer refcount for a pipe endpoint when an fd referring to
/// it is duplicated (dup, dup2, fork inheritance). No-op for non-pipe fds.
/// Caller must NOT hold the FD_TABLES lock (we take PIPE_RINGS here).
fn pipe_ref_inc(kind: &VnodeKind) {
    if let VnodeKind::Pipe { ring, is_write } = kind {
        let mut rings = PIPE_RINGS.lock();
        if *is_write { rings[*ring].writers += 1; } else { rings[*ring].readers += 1; }
    }
}

/// Drop one reference to a pipe endpoint (a close, or a dup2 that overwrites an
/// already-open fd). Saturating so a stray double-close can never underflow.
/// When the last endpoint on BOTH sides is gone, reset the ring: the allocator
/// in handle_pipe only reuses slots with `count == 0`, so a pipe abandoned with
/// unread data would otherwise leak its slot for the lifetime of the system.
fn pipe_drop_ref(rings: &mut [PipeRing; MAX_PIPES], ring: usize, is_write: bool) {
    if is_write { rings[ring].writers = rings[ring].writers.saturating_sub(1); }
    else        { rings[ring].readers = rings[ring].readers.saturating_sub(1); }
    // The last writer or reader going away is a new pollable edge (EOF/HUP on
    // the read end, EPIPE/ERR on the write end) — advance the seq so epoll
    // re-fires it once, edge-triggered.
    if (is_write && rings[ring].writers == 0) || (!is_write && rings[ring].readers == 0) {
        rings[ring].seq = rings[ring].seq.wrapping_add(1);
    }
    if rings[ring].readers == 0 && rings[ring].writers == 0 {
        rings[ring].read_pos  = 0;
        rings[ring].write_pos = 0;
        rings[ring].count     = 0;
    }
}

fn pipe_ref_dec(kind: &VnodeKind) {
    if let VnodeKind::Pipe { ring, is_write } = kind {
        pipe_drop_ref(&mut PIPE_RINGS.lock(), *ring, *is_write);
    }
}

// ── eventfd counters ──────────────────────────────────────────────────────────

const MAX_EVENTFDS: usize = 16;
// u64::MAX = free slot sentinel.
static EVENTFD_COUNTERS: Mutex<[u64; MAX_EVENTFDS]> = Mutex::new([u64::MAX; MAX_EVENTFDS]);
/// Per-eventfd monotonic event counter, bumped on every write. mio registers
/// its waker eventfd edge-triggered (EPOLLET) and never drains it, so its
/// POLLIN level stays high forever; the epoll layer compares this seq to
/// re-fire once per wake() instead of spinning on the stuck level.
static EVENTFD_SEQ: Mutex<[u64; MAX_EVENTFDS]> = Mutex::new([0u64; MAX_EVENTFDS]);

// ── /dev/urandom LFSR ─────────────────────────────────────────────────────────

static LFSR_STATE: Mutex<u64> = Mutex::new(0xdeadbeef_cafebabe);

fn lfsr_next() -> u8 {
    let mut state = LFSR_STATE.lock();
    *state ^= sched::ticks().wrapping_mul(0x9e3779b97f4a7c15); // mix ticks for entropy
    let lsb = *state & 1;
    *state >>= 1;
    if lsb != 0 { *state ^= 0xB400000000000000; }
    (*state & 0xFF) as u8
}

// ── timerfd pool ──────────────────────────────────────────────────────────────

const MAX_TIMERFDS: usize = 16;

#[derive(Clone, Copy)]
struct TimerFdEntry {
    armed:          bool,
    deadline_ticks: u64,   // absolute tick when next expiration fires
    interval_ticks: u64,   // 0 = one-shot
    expirations:    u64,   // accumulated unread expiration count
}

impl TimerFdEntry {
    const fn free() -> Self {
        Self { armed: false, deadline_ticks: 0, interval_ticks: 0, expirations: 0 }
    }
    const fn is_free(&self) -> bool { !self.armed && self.deadline_ticks == 0 && self.expirations == 0 }
}

static TIMERFD_POOL: Mutex<[TimerFdEntry; MAX_TIMERFDS]> =
    Mutex::new([const { TimerFdEntry::free() }; MAX_TIMERFDS]);

/// Recompute a timerfd's pending-expiration count against the current tick,
/// folding any missed periods into `expirations` and rearming the deadline —
/// the same catch-up logic `handle_read`'s `TimerFd` arm used to run inline.
/// Non-consuming: callers that read the count (rather than just probing
/// readiness) are responsible for resetting `expirations` to 0 afterward.
/// Shared with `handle_ioctl`'s `FIONREAD` and `handle_poll` so neither can
/// under-report an expiry that hasn't been read yet.
fn timerfd_poll_expirations(slot: usize) -> u64 {
    let now = sched::ticks();
    let mut pool = TIMERFD_POOL.lock();
    let e = &mut pool[slot];
    if e.armed && now >= e.deadline_ticks {
        let elapsed = now - e.deadline_ticks;
        let extra = if e.interval_ticks > 0 { elapsed / e.interval_ticks + 1 } else { 1 };
        e.expirations += extra;
        if e.interval_ticks > 0 {
            e.deadline_ticks += extra * e.interval_ticks;
        } else {
            e.armed = false;
        }
    }
    e.expirations
}

// ── FD table ─────────────────────────────────────────────────────────────────

const MAX_PROCS: usize = 64;
const MAX_FDS:   usize = 64;
const O_CLOEXEC: u32   = 0x8_0000;

#[derive(Clone, Copy)]
struct FdEntry {
    kind:   VnodeKind,
    flags:  u32,
    in_use: bool,
}

impl FdEntry {
    const fn empty() -> Self {
        Self { kind: VnodeKind::None, flags: 0, in_use: false }
    }
}

#[derive(Clone, Copy)]
struct ProcFdTable {
    pid:    u32,
    fds:    [FdEntry; MAX_FDS],
    in_use: bool,
}

impl ProcFdTable {
    const fn empty() -> Self {
        Self { pid: 0, fds: [const { FdEntry::empty() }; MAX_FDS], in_use: false }
    }

    fn alloc_fd(&mut self) -> Option<usize> {
        // Never hand out fds 0-2: the kernel's sys_read/sys_write fast paths
        // hardwire them to the serial console before consulting the VFS, so a
        // vnode on those numbers would be shadowed (a pipe write end on fd 1
        // writes to the UART, its data never reaches the ring). Processes
        // whose table is created fresh (no inherited entries) would otherwise
        // get exactly that from their first pipe()/open().
        self.fds.iter().enumerate().skip(3)
            .find(|(_, f)| !f.in_use).map(|(i, _)| i)
    }
}

static FD_TABLES: Mutex<[ProcFdTable; MAX_PROCS]> =
    Mutex::new([const { ProcFdTable::empty() }; MAX_PROCS]);

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

struct RamEntry { path: &'static [u8], data: &'static [u8] }

static RAMFS: &[RamEntry] = &[
    // /dev virtual devices (zero-length placeholder; open is intercepted above)
    RamEntry { path: b"/dev/null",    data: b"" },
    RamEntry { path: b"/dev/zero",    data: b"" },
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
    RamEntry { path: b"/dev/fb0",     data: b"" },
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
    RamEntry { path: b"/proc/cpuinfo",
               data: b"processor\t: 0\nmodel name\t: Leandros Virtual CPU\ncpu MHz\t\t: 1000.000\n\
                       cache size\t: 4096 KB\nflags\t\t: fpu vme de pse tsc msr pae mce\n" },
    RamEntry { path: b"/proc/filesystems",
               data: b"nodev\ttmpfs\nnodev\tramfs\nnodev\tprocfs\n\text2\n" },
    // /proc/mounts is generated from list_mounts() by gen_proc_system_content
    // (see the "/proc/mounts" arm there) — a static entry here would shadow
    // the generated one and go stale the moment a real filesystem is
    // mounted, which is exactly what broke `df`.
    RamEntry { path: b"/proc/net/dev",
               data: b"Inter-|   Receive                                       |  Transmit\n\
                       face |bytes packets errs drop fifo frame compressed multicast\
                       |bytes packets errs drop fifo colls carrier compressed\n\
                   lo:      0       0    0    0    0     0          0         0       0       0    0    0    0     0       0          0\n" },
    RamEntry { path: b"/proc/net/if_inet6",  data: b"" },
    RamEntry { path: b"/proc/net/fib_trie",  data: b"Main:\n  +-- 0.0.0.0/0\n" },
    RamEntry { path: b"/proc/sys/kernel/hostname",   data: b"leandros\n" },
    RamEntry { path: b"/proc/sys/kernel/ostype",     data: b"Linux\n" },
    RamEntry { path: b"/proc/sys/kernel/osrelease",  data: b"6.0.0-leandros\n" },
    RamEntry { path: b"/proc/sys/vm/overcommit_memory", data: b"0\n" },
];

/// Known directories for getdents64.
static RAMFS_DIRS: &[&[u8]] = &[
    b"/",
    b"/etc",
    b"/dev",
    b"/proc",
    b"/bin",
    b"/tmp",
    b"/dev/shm",
    b"/run",
    b"/run/user",
    b"/run/user/0",
    b"/mnt",
    b"/home",
    b"/root",
    b"/proc/net",
    b"/proc/sys",
    b"/proc/sys/kernel",
    b"/proc/sys/vm",
];

// ── Server port ───────────────────────────────────────────────────────────────

static SERVER_PORT: Mutex<u32> = Mutex::new(u32::MAX);

/// Initialise the VFS server and return its IPC port ID.
pub fn init(owner_pid: u32) -> Option<u32> {
    let port_id = port::create(owner_pid)?;
    *SERVER_PORT.lock() = port_id;
    
    // Register IPC handler to respond to PINGs (prevents deadlocks during discovery scans)
    port::register_handler(port_id, |msg, pid, _target| {
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
                in_use: true
            };
        }
    }

    // Register PID 1 with stdin/stdout/stderr → /dev/null.
    let mut tbls = FD_TABLES.lock();
    for slot in tbls.iter_mut() {
        if !slot.in_use {
            slot.in_use = true;
            slot.pid    = 1;
            for fd in 0..3 {
                slot.fds[fd] = FdEntry { kind: VnodeKind::DevNull, flags: 0, in_use: true };
            }
            break;
        }
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
    // Check tmpfs dirs.
    let path = match tmpfs_path(&pbuf[..plen]) { Some(p) => p, None => return false };
    let tmp = TMP_FILES.lock();
    tmp_find(&tmp[..], path).map_or(false, |i| tmp[i].is_dir)
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
        VFS_LCHMOD | VFS_LCHOWN                                 => (Some((0, false)), None),
        VFS_STAT | VFS_STATFS | VFS_CHMOD | VFS_CHOWN           => (Some((0, true)), None),
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
fn rewrite_one(msg: &mut Message, spec: Option<(usize, bool)>, buf: &mut [u8; 257])
    -> Result<(), i32>
{
    let (idx, follow) = match spec { Some(s) => s, None => return Ok(()) };
    let ptr = arg(msg, idx) as usize;
    let (raw, rlen) = match read_cstr_raw(ptr) { Some(r) => r, None => return Ok(()) };
    let path = strip_trailing_slash(&raw[..rlen]);
    if !is_tmp_path(path) { return Ok(()); }

    let mut resolved = [0u8; 256];
    let n = tmp_resolve_links(path, follow, &mut resolved)?;
    if resolved[..n] == raw[..rlen] { return Ok(()); }

    buf[..n].copy_from_slice(&resolved[..n]);
    buf[n] = 0; // handlers scan for the NUL
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

    // Symlink resolution happens here, once, for every path-taking operation —
    // rather than in each of the fifteen handlers that would otherwise each
    // need their own copy of the walk. The rewritten paths live in this
    // frame, and the caller stays blocked in `call_port` for the whole round
    // trip, so they outlive every server that reads them (the same lifetime
    // argument `KPath` makes on the kernel side).
    let (p0, p1) = path_args(&msg);
    if p0.is_some() || p1.is_some() {
        let mut b0 = [0u8; 257];
        let mut b1 = [0u8; 257];
        let mut m = *msg;
        if let Err(e) = rewrite_one(&mut m, p0, &mut b0) { return err_reply(e); }
        if let Err(e) = rewrite_one(&mut m, p1, &mut b1) { return err_reply(e); }
        return dispatch(&m, caller_pid);
    }
    dispatch(msg, caller_pid)
}

fn dispatch(msg: &Message, caller_pid: u32) -> Message {
    match msg.tag {
        VFS_OPEN         => handle_open(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32, arg(msg,2) as u32),
        VFS_READ         => handle_read(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_WRITE        => handle_write(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize, arg(msg,2) as usize),
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
        VFS_UNLINK       => handle_unlink(arg(msg,0) as usize),
        VFS_MKDIR        => handle_mkdir(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        VFS_MKNOD        => handle_mknod(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        VFS_FTRUNCATE    => handle_ftruncate(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_FSYNC        => handle_fsync(caller_pid, arg(msg,0) as usize),
        VFS_SYNC         => handle_sync(),
        VFS_RENAME       => handle_rename(arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_FD_PATH      => handle_fd_path(caller_pid, arg(msg,0) as usize,
                                            arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_EVENTFD          => handle_eventfd(caller_pid, arg(msg,0) as u64),
        VFS_TIMERFD_CREATE   => handle_timerfd_create(caller_pid),
        VFS_TIMERFD_SETTIME  => handle_timerfd_settime(caller_pid, arg(msg,0) as usize,
                                                        arg(msg,1) as u64, arg(msg,2) as u64),
        VFS_TIMERFD_GETTIME  => handle_timerfd_gettime(caller_pid, arg(msg,0) as usize,
                                                        arg(msg,1) as usize),
        VFS_IOCTL            => handle_ioctl(caller_pid, arg(msg,0) as usize,
                                              arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_RMDIR            => handle_rmdir(arg(msg,0) as usize),
        VFS_FLOCK            => handle_flock(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        VFS_CHMOD            => handle_chmod(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32, true),
        VFS_LCHMOD           => handle_chmod(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32, false),
        VFS_FCHMOD           => handle_fchmod(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        VFS_CHOWN            => handle_chown(caller_pid, arg(msg,0) as usize,
                                              arg(msg,1) as u32, arg(msg,2) as u32, true),
        VFS_LCHOWN           => handle_chown(caller_pid, arg(msg,0) as usize,
                                              arg(msg,1) as u32, arg(msg,2) as u32, false),
        VFS_FCHOWN           => handle_fchown(caller_pid, arg(msg,0) as usize,
                                               arg(msg,1) as u32, arg(msg,2) as u32),
        VFS_POLL             => handle_poll(caller_pid, arg(msg,0) as usize),
        VFS_PIVOT_ROOT       => handle_pivot_root(arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_SYMLINK          => handle_symlink(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_READLINK         => handle_readlink(arg(msg,0) as usize, arg(msg,1) as usize,
                                                arg(msg,2) as usize),
        VFS_LINK             => handle_link(arg(msg,0) as usize, arg(msg,1) as usize),
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
        VFS_ACCESS           => handle_access(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
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

fn should_lookup_ramfs<'a>(path: &'a [u8]) -> Option<&'a [u8]> {
    if path.starts_with(b"/dev/") || path.starts_with(b"/proc/")
       || path.starts_with(b"/tmp/") || path.starts_with(b"/etc/")
       || path == b"/dev" || path == b"/proc" || path == b"/tmp" || path == b"/etc"
       // The /run/user tmpfs mount (Wayland + D-Bus sockets). /dev/shm rides
       // the /dev/ prefix above; /tmp rides /tmp/. Intercept /run/user before
       // the mount table so it lands on tmpfs, not the pivoted F2FS root.
       || path.starts_with(b"/run/user") || path == b"/run"
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
/// and `/run/user/0` (Wayland + D-Bus sockets) are the K1 additions.
static TMPFS_ROOTS: &[&[u8]] = &[b"/tmp", b"/dev/shm", b"/run/user/0"];

/// True when `path` names a tmpfs mount root exactly.
fn is_tmpfs_root(path: &[u8]) -> bool {
    TMPFS_ROOTS.iter().any(|&r| r == path)
}

/// The tmpfs root that owns `path` (the root itself, or a descendant under it),
/// or `None` when no tmpfs mount owns it.
fn tmpfs_root_of(path: &[u8]) -> Option<&'static [u8]> {
    TMPFS_ROOTS.iter().copied().find(|&r| {
        path == r || (path.len() > r.len() && path.starts_with(r) && path[r.len()] == b'/')
    })
}

/// Return true if `path` lives under any tmpfs mount root (root or descendant).
fn is_tmp_path(path: &[u8]) -> bool {
    tmpfs_root_of(path).is_some()
}

/// S_IFDIR mode (type | permission bits) for a RAMFS_DIRS pseudo-directory.
/// The K1 tmpfs mount roots carry conventional perms: `/dev/shm` is
/// world-writable + sticky (1777, like a real shm mount), `/run/user/0` is
/// private to its owner (0700). Everything else keeps the historical 0755.
fn ramfs_dir_mode(dir: &[u8]) -> u32 {
    match dir {
        b"/dev/shm"    => 0o041777,
        b"/run/user/0" => 0o040700,
        _              => 0o040755,
    }
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
fn tmp_find(tmp: &[TmpFileEntry], path: &[u8]) -> Option<usize> {
    tmp.iter().position(|e| {
        e.in_use && !e.ephemeral && e.path_len == path.len() && &e.path[..e.path_len] == path
    })
}

/// True when `path` names an existing directory that entries may be created
/// under: "/tmp" always, otherwise an in-use `is_dir` pool entry.
fn tmp_dir_exists(tmp: &[TmpFileEntry], path: &[u8]) -> bool {
    is_tmpfs_root(path) || tmp_find(tmp, path).map_or(false, |i| tmp[i].is_dir)
}

/// True when any pool entry (other than `skip`) lives under directory `dir`.
fn tmp_has_descendants(tmp: &[TmpFileEntry], dir: &[u8], skip: usize) -> bool {
    tmp.iter().enumerate().any(|(i, e)| {
        i != skip && e.in_use && !e.ephemeral
            && e.path_len > dir.len()
            && &e.path[..dir.len()] == dir
            && e.path[dir.len()] == b'/'
    })
}

/// Overwrite an entry's stored path. Callers must have length-checked already.
fn tmp_set_path(e: &mut TmpFileEntry, path: &[u8]) {
    e.path = [0u8; MAX_TMP_PATH];
    e.path[..path.len()].copy_from_slice(path);
    e.path_len = path.len();
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
//   to touch `.data`, `.len`, `.mode`, `.uid` or `.gid`. Map it through
//   `tmp_owner()` first. Only path-shaped operations (rename, getdents,
//   lookup) legitimately use the un-mapped index.

/// Map a pool index to the slot that actually owns the bytes.
fn tmp_owner(tmp: &[TmpFileEntry], idx: usize) -> usize {
    let to = tmp[idx].link_to;
    if to == usize::MAX || to >= tmp.len() || !tmp[to].in_use { idx } else { to }
}

/// Number of aliases pointing at data-owning slot `owner`.
fn tmp_alias_count(tmp: &[TmpFileEntry], owner: usize) -> usize {
    tmp.iter().enumerate().filter(|(i, e)| {
        *i != owner && e.in_use && e.link_to == owner
    }).count()
}

/// `st_nlink` for the file owned by slot `idx`.
///
/// An `ephemeral` owner is a *nameless* inode (see `tmp_drop_name`), so it
/// contributes nothing — which is how `ln a b; rm a; stat b` correctly reports
/// 1 rather than 2.
fn tmp_nlink(tmp: &[TmpFileEntry], idx: usize) -> u64 {
    let owner = tmp_owner(tmp, idx);
    let named = if tmp[owner].ephemeral { 0 } else { 1 };
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
/// `open_fds` is a `tmp_open_fd_mask()` snapshot taken before TMP_FILES was
/// locked; without it, `fd = open(p); unlink(p)` freed the pool slot out from
/// under `fd`, and the next creat() handed the same slot to an unrelated file.
/// That is precisely the create-then-unlink idiom tempfile(3) uses for
/// anonymous temporaries, so it is on the hot path for `tac`, `sort -o` and
/// every other tool that buffers through a temp file.
fn tmp_drop_name(tmp: &mut [TmpFileEntry], idx: usize, open_fds: u128) {
    let referenced = |i: usize| i < MAX_TMP_FILES && open_fds & (1u128 << i) != 0;
    let owner = tmp_owner(tmp, idx);
    if owner != idx {
        // An alias. Free it (an alias never owns a VMO — the VMO is keyed on
        // `owner`), and collect the inode if that was the last name and the
        // owner had already lost its own.
        tmp[idx] = TmpFileEntry::empty();
        if tmp[owner].ephemeral && tmp_alias_count(tmp, owner) == 0 && !referenced(owner) {
            vmo_free_slot(owner); // release the inode's VMO frames (K1)
            tmp[owner] = TmpFileEntry::empty();
        }
        return;
    }
    if tmp_alias_count(tmp, idx) > 0 || referenced(idx) {
        tmp[idx].ephemeral = true;
    } else {
        vmo_free_slot(idx); // release the inode's VMO frames (K1)
        tmp[idx] = TmpFileEntry::empty();
    }
}

/// Bitmask of tmpfs pool slots still referenced by an open descriptor.
///
/// Lock order is the established FD_TABLES → TMP_FILES, so callers must invoke
/// this *before* taking the TMP_FILES lock and pass the result down.
fn tmp_open_fd_mask() -> u128 {
    let tbls = FD_TABLES.lock();
    let mut mask: u128 = 0;
    for t in tbls.iter().filter(|t| t.in_use) {
        for f in t.fds.iter().filter(|f| f.in_use) {
            if let VnodeKind::TmpFile { idx, .. } = f.kind {
                if idx < MAX_TMP_FILES { mask |= 1u128 << idx; }
            }
        }
    }
    mask
}

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
fn normalize_abs_floor(src: &[u8], out: &mut [u8; 256], floor: usize) -> usize {
    let floor = floor.max(1);
    let mut len = 1usize;
    out[0] = b'/';
    for comp in src.split(|&b| b == b'/') {
        if comp.is_empty() || comp == b"." { continue; }
        if comp == b".." {
            if len > floor {
                let mut last = len - 1;
                while last > 0 && out[last] != b'/' { last -= 1; }
                let clamped = if last == 0 { 1 } else { last };
                len = if clamped < floor { floor } else { clamped };
            }
            continue;
        }
        if len > 1 { if len >= 255 { break; } out[len] = b'/'; len += 1; }
        let n = comp.len().min(255 - len);
        out[len..len + n].copy_from_slice(&comp[..n]);
        len += n;
    }
    len
}

fn normalize_abs(src: &[u8], out: &mut [u8; 256]) -> usize {
    normalize_abs_floor(src, out, 1)
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
fn caller_jail_tmp(out: &mut [u8; 128]) -> usize {
    let mut host = [0u8; 256];
    let n = sched::current_root(host.as_mut_ptr(), 256);
    if n <= 1 { return 0; }
    let n = (n as usize).min(255);
    if !is_tmp_path(&host[..n]) { return 0; }
    let take = n.min(128);
    out[..take].copy_from_slice(&host[..take]);
    take
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
fn tmp_resolve_links(input: &[u8], follow_final: bool, out: &mut [u8; 256]) -> Result<usize, i32> {
    // Jail root on tmpfs (empty = not confined here). An absolute symlink target
    // must re-anchor here, not at the tmpfs root, or a link inside a jail rooted
    // on tmpfs (`chroot /tmp/jail`) can name a path above the jail. Unjailed,
    // `jlen == 0` and `floor == 1`, so everything below is the old behaviour.
    let mut jail = [0u8; 128];
    let jlen = caller_jail_tmp(&mut jail);
    let floor = if jlen > 1 { jlen } else { 1 };

    let mut cur = [0u8; 256];
    let mut cur_len = normalize_abs_floor(input, &mut cur, floor);
    let mut hops = 0u32;

    loop {
        let path = &cur[..cur_len];
        // Left the tmpfs namespace (an absolute target pointing elsewhere) —
        // stop resolving and let the caller route the result.
        if !is_tmp_path(path) { break; }

        // Locate the first component that is a symlink. `comp_start` is the
        // index of the '/' preceding it, `comp_end` one past its last byte.
        let hit = {
            let tmp = TMP_FILES.lock();
            // Skip the mount-root prefix ("/tmp", "/dev/shm", "/run/user/0") so
            // component scanning starts at the first path element under it.
            let mut comp_start = tmpfs_root_of(path).map(|r| r.len()).unwrap_or(4);
            let mut found = None;
            while comp_start < path.len() {
                let mut comp_end = comp_start + 1;
                while comp_end < path.len() && path[comp_end] != b'/' { comp_end += 1; }
                let is_last = comp_end == path.len();
                if !(is_last && !follow_final) {
                    if let Some(idx) = tmp_find(&tmp[..], &path[..comp_end]) {
                        if tmp[idx].is_link {
                            // A link body is a path, so 256 bytes is the whole
                            // range — copying MAX_TMP_SIZE here would put a
                            // 32 KiB buffer on the kernel stack per hop.
                            let mut target = [0u8; 256];
                            let tlen = tmp[idx].len.min(255);
                            target[..tlen].copy_from_slice(&tmp[idx].data[..tlen]);
                            found = Some((comp_start, comp_end, target, tlen));
                            break;
                        }
                    }
                }
                comp_start = comp_end;
            }
            found
        };

        let (comp_start, comp_end, target, tlen) = match hit {
            Some(h) => h,
            None    => break, // fully resolved
        };

        hops += 1;
        if hops > SYMLINK_MAX_HOPS { return Err(-40); } // ELOOP

        // Splice: [prefix] + target + [remainder]. An absolute target replaces
        // the prefix outright; a relative one hangs off the symlink's own
        // parent directory (`path[..comp_start]`).
        let mut next = [0u8; 256];
        let mut n = 0usize;
        let mut push = |bytes: &[u8], n: &mut usize| -> bool {
            if *n + bytes.len() > 255 { return false; }
            next[*n..*n + bytes.len()].copy_from_slice(bytes);
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

        cur_len = normalize_abs_floor(&next[..n], &mut cur, floor);
    }

    out[..cur_len].copy_from_slice(&cur[..cur_len]);
    Ok(cur_len)
}

/// Release a pool slot backing an ephemeral /proc snapshot once no fd refers
/// to it any more. Without this, every open of /proc/self/* burned one of the
/// 32 pool slots forever, and the pool exhaustion surfaced much later as an
/// unexplained ENOSPC from creat()/mkdir().
///
/// Lock order is the established FD_TABLES → TMP_FILES.
fn tmp_release_ephemeral(idx: usize) {
    let tbls = FD_TABLES.lock();
    let still_referenced = tbls.iter().any(|t| {
        t.in_use && t.fds.iter().any(|f| {
            f.in_use && matches!(f.kind, VnodeKind::TmpFile { idx: i, .. } if i == idx)
        })
    });
    let mut tmp = TMP_FILES.lock();
    // `ephemeral` also marks a hard-linked inode whose own name was unlinked
    // while other names survive. Those are still reachable through an alias,
    // so an fd going away must not collect them.
    if !still_referenced && tmp[idx].in_use && tmp[idx].ephemeral
        && tmp_alias_count(&tmp[..], idx) == 0
    {
        // A memfd whose name was unlinked while an fd stayed open lands here on
        // the final close — release its VMO frames before freeing the slot (K1).
        vmo_free_slot(idx);
        tmp[idx] = TmpFileEntry::empty();
    }
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

const TMP_BUF_SIZE: usize = 512;

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
        p = write_lit(buf, p, b" rw 0 0\n");
        if p >= buf.len() { p = start; break; }
    }
    p
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
    let mut mount_id: u32 = 20;
    for m in list_mounts().iter() {
        if !m.in_use { continue; }
        let start = p;
        p = write_u32(buf, p, mount_id);
        p = write_lit(buf, p, b" 1 0:");
        p = write_u32(buf, p, mount_id);
        p = write_lit(buf, p, b" / ");
        p = write_lit(buf, p, m.prefix.as_bytes());
        p = write_lit(buf, p, b" rw,relatime - ");
        p = write_lit(buf, p, m.fstype.as_bytes());
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, m.device.as_bytes());
        p = write_lit(buf, p, b" rw\n");
        if p >= buf.len() { p = start; break; }
        mount_id += 1;
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
    let idx = tmp.iter().position(|e| !e.in_use)?;
    tmp[idx] = TmpFileEntry::empty();
    tmp[idx].in_use    = true;
    tmp[idx].ephemeral = true;
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
    tmp[idx].path[..fp_len].copy_from_slice(&fake_path[..fp_len]);
    tmp[idx].path_len = fp_len;
    let copy = len.min(TMP_BUF_SIZE);
    tmp[idx].data[..copy].copy_from_slice(&buf[..copy]);
    tmp[idx].len = copy;
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: false })
}

/// Generate dynamic /proc/ system-wide entries (meminfo, uptime, loadavg, stat).
fn gen_proc_system(path: &[u8]) -> Option<VnodeKind> {
    let mut buf = [0u8; TMP_BUF_SIZE];
    let len = gen_proc_system_content(path, &mut buf)?;
    let mut tmp = TMP_FILES.lock();
    let idx = tmp.iter().position(|e| !e.in_use)?;
    tmp[idx] = TmpFileEntry::empty();
    tmp[idx].in_use = true;
    tmp[idx].ephemeral = true;
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
    tmp[idx].path[..fl].copy_from_slice(&fake_path[..fl]);
    tmp[idx].path_len = fl;
    let copy = len.min(TMP_BUF_SIZE);
    tmp[idx].data[..copy].copy_from_slice(&buf[..copy]);
    tmp[idx].len = copy;
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: false })
}

fn gen_proc_system_content(path: &[u8], buf: &mut [u8; TMP_BUF_SIZE]) -> Option<usize> {
    let ticks = sched::ticks();
    let uptime_sec  = ticks / 100;
    let uptime_frac = (ticks % 100) / 10; // tenths of a second

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
        let mut p = 0;
        p = write_lit(buf, p, b"0.00 0.00 0.00 1/1 ");
        p = write_u32(buf, p, sched::current_pid());
        p = write_lit(buf, p, b"\n");
        return Some(p);
    }

    if path == b"/proc/meminfo" {
        let total = mm::buddy::total_pages() * 4; // pages → KiB
        let free  = mm::buddy::free_pages()  * 4;
        let used  = total.saturating_sub(free);
        let mut p = 0;
        p = write_lit(buf, p, b"MemTotal:       ");
        p = write_u32(buf, p, total as u32);
        p = write_lit(buf, p, b" kB\nMemFree:        ");
        p = write_u32(buf, p, free as u32);
        p = write_lit(buf, p, b" kB\nMemAvailable:   ");
        p = write_u32(buf, p, free as u32);
        p = write_lit(buf, p, b" kB\nBuffers:        0 kB\nCached:         ");
        p = write_u32(buf, p, used as u32);
        p = write_lit(buf, p, b" kB\nSwapTotal:      0 kB\nSwapFree:       0 kB\n");
        return Some(p);
    }

    if path == b"/proc/stat" {
        let mut p = 0;
        p = write_lit(buf, p, b"cpu  0 0 0 ");
        p = write_u32(buf, p, (uptime_sec * 100) as u32); // idle jiffies
        p = write_lit(buf, p, b" 0 0 0 0 0 0\ncpu0 0 0 0 ");
        p = write_u32(buf, p, (uptime_sec * 100) as u32);
        p = write_lit(buf, p, b" 0 0 0 0 0 0\nbtime ");
        // Boot time = now − uptime (fake: use 0)
        p = write_lit(buf, p, b"0\nprocesses 1\nprocs_running 1\n");
        return Some(p);
    }

    if path == b"/proc/self" {
        // Symlink target: just return pid as a string (used by some programs as a dir)
        let mut p = 0;
        p = write_u32(buf, p, sched::current_pid());
        return Some(p);
    }

    if path == b"/proc/mounts" {
        // Legacy mtab-format mount table, generated from the live mount
        // registry — see write_mtab_lines for the field layout and why.
        return Some(write_mtab_lines(buf, 0));
    }

    None
}

/// Generate dynamic content for a /proc/self/<name> path.
/// Allocates a TmpFile slot, writes the content, and returns the vnode.
fn gen_proc_self(pid: u32, path: &[u8]) -> Option<VnodeKind> {
    let mut buf = [0u8; TMP_BUF_SIZE];
    let len = gen_proc_self_content(pid, path, &mut buf)?;

    let mut tmp = TMP_FILES.lock();
    let idx = tmp.iter().position(|e| !e.in_use)?;
    tmp[idx] = TmpFileEntry::empty();
    tmp[idx].in_use    = true;
    tmp[idx].is_dir    = false;
    tmp[idx].ephemeral = true;
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
    tmp[idx].path[..fp_len].copy_from_slice(&fake_path[..fp_len]);
    tmp[idx].path_len = fp_len;
    // Copy the generated content into the data buffer.
    let copy = len.min(TMP_BUF_SIZE);
    tmp[idx].data[..copy].copy_from_slice(&buf[..copy]);
    tmp[idx].len = copy;
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: false })
}

fn gen_proc_self_content(pid: u32, path: &[u8], buf: &mut [u8; TMP_BUF_SIZE]) -> Option<usize> {
    let ppid = sched::current_ppid();
    let pgid = sched::current_pgid();
    let ticks = sched::ticks();
    let uptime_sec = ticks / 100;

    if path == b"/proc/self/status" || path.ends_with(b"/status") {
        let mut p = 0;
        p = write_lit(buf, p, b"Name:\tleandros\nState:\tR (running)\nPid:\t");
        p = write_u32(buf, p, pid);
        p = write_lit(buf, p, b"\nPPid:\t");
        p = write_u32(buf, p, ppid);
        p = write_lit(buf, p, b"\nPGid:\t");
        p = write_u32(buf, p, pgid);
        p = write_lit(buf, p, b"\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n");
        p = write_lit(buf, p, b"VmRSS:\t4096 kB\nVmSize:\t8192 kB\nThreads:\t1\n");
        return Some(p);
    }

    if path == b"/proc/self/stat" || path.ends_with(b"/stat") {
        // Format: pid (comm) state ppid pgid ...
        let mut p = 0;
        p = write_u32(buf, p, pid);
        p = write_lit(buf, p, b" (leandros) R ");
        p = write_u32(buf, p, ppid);
        p = write_lit(buf, p, b" ");
        p = write_u32(buf, p, pgid);
        p = write_lit(buf, p, b" 0 0 0 0 0 0 0 0 0 0 0 0 0 0 20 0 1 0 ");
        p = write_u32(buf, p, uptime_sec as u32);
        p = write_lit(buf, p, b" 8388608 2048 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 0\n");
        return Some(p);
    }

    if path == b"/proc/self/cmdline" || path.ends_with(b"/cmdline") {
        let s = b"leandros\x00";
        let copy = s.len().min(TMP_BUF_SIZE);
        buf[..copy].copy_from_slice(&s[..copy]);
        return Some(copy);
    }

    if path == b"/proc/self/maps" || path.ends_with(b"/maps") {
        // Return minimal maps (empty — no VMAs exposed)
        return Some(0);
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
        // see write_mountinfo_lines for the field layout and why.
        return Some(write_mountinfo_lines(buf, 0));
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

    let kind = if let Some(lookup_path) = should_lookup_ramfs(path) {
        if lookup_path == b"/dev/null" {
            VnodeKind::DevNull
        } else if lookup_path == b"/dev/zero" {
            VnodeKind::DevZero
        } else if lookup_path == b"/dev/urandom" || lookup_path == b"/dev/random" {
            VnodeKind::DevUrandom
        } else if lookup_path == b"/dev/stdin" {
            VnodeKind::DevStdio { target_fd: 0 }
        } else if lookup_path == b"/dev/stdout" {
            VnodeKind::DevStdio { target_fd: 1 }
        } else if lookup_path == b"/dev/stderr" {
            VnodeKind::DevStdio { target_fd: 2 }
        } else if lookup_path == b"/dev/tty" || lookup_path == b"/dev/console" {
            // The controlling terminal: a console proxy, exactly like a
            // dup'd stdin (crossterm opens this when it decides stdin isn't
            // usable; a plain empty RamFile here returned instant EOF and
            // starved its input reader).
            VnodeKind::DevStdio { target_fd: 0 }
        } else if lookup_path == b"/dev/fb0" {
            VnodeKind::DevFb { pos: 0 }
        } else if is_tmp_path(path) && !is_tmpfs_root(path) {
            // ── Writable tmpfs file (/tmp, /dev/shm, /run/user/0) ─────────────────
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
            let want_write = accmode == O_WRONLY || accmode == O_RDWR;
            let euid = sched::euid_of(pid);
            let egid = sched::egid_of(pid);

            let mut tmp = TMP_FILES.lock();
            // A mkdir'd tmpfs directory opens as a directory vnode — the
            // TmpFile slot it already has, with `pos` doubling as the
            // getdents64 cursor. Before this, the file lookup below skipped
            // is_dir entries entirely, so opendir("/tmp/sub") was ENOENT and
            // O_CREAT on it would have shadowed the directory with a file.
            if let Some(idx) = tmp_find(&tmp[..], path).filter(|&i| tmp[i].is_dir) {
                if excl { return err_reply(-17); } // EEXIST — beats EISDIR, as on Linux
                if flags & (O_WRONLY | O_RDWR) != 0 { return err_reply(-21); } // EISDIR
                {
                    let e = &tmp[idx];
                    let meta = tmp_meta(e);
                    let acl = xattr::find(&e.xattr, xattr::IDX_ACL_ACCESS, b"");
                    if !xattr::access_check(&meta, euid, egid, acl, true, false, false) {
                        return err_reply(-13); // EACCES
                    }
                }
                VnodeKind::TmpFile { idx, pos: 0, writable: false }
            } else {
            // Look for an existing entry. A hard-link alias carries no bytes,
            // so the fd must be bound to the slot that owns them — do that
            // once, here, and every downstream read/write/lseek/ftruncate/
            // fstat keeps working on a bare pool index unchanged.
            let existing = tmp_find(&tmp[..], path).map(|i| tmp_owner(&tmp[..], i));
            match existing {
                Some(idx) => {
                    // O_CREAT|O_EXCL on an existing file: EEXIST, checked
                    // before access permissions so an unreadable file still
                    // reports "already there" rather than leaking EACCES.
                    if excl { return err_reply(-17); } // EEXIST
                    {
                        let e = &tmp[idx];
                        let meta = tmp_meta(e);
                        let acl = xattr::find(&e.xattr, xattr::IDX_ACL_ACCESS, b"");
                        if !xattr::access_check(&meta, euid, egid, acl, want_read, want_write, false) {
                            return err_reply(-13); // EACCES
                        }
                    }
                    if trunc { tmp[idx].len = 0; }
                    let pos = if writable && trunc { 0 }
                              else if flags & O_APPEND != 0 { tmp[idx].len }
                              else { 0 };
                    VnodeKind::TmpFile { idx, pos, writable: writable || create }
                }
                None if create => {
                    // The parent directory must already exist, exactly as on
                    // any real fs: `touch /tmp/nodir/f` is ENOENT, not a file
                    // named "/tmp/nodir/f" that no directory can enumerate.
                    match tmp_parent(path) {
                        Some(p) if tmp_dir_exists(&tmp[..], p) => {}
                        _ => return err_reply(-2), // ENOENT
                    }
                    if path.len() > MAX_TMP_PATH - 1 { return err_reply(-36); }
                    // Allocate a new slot.
                    match tmp.iter().position(|e| !e.in_use) {
                        Some(idx) => {
                            tmp[idx] = TmpFileEntry::empty();
                            tmp[idx].in_use   = true;
                            tmp[idx].is_dir   = false;
                            tmp[idx].mode     = mode & 0o777 & !sched::umask(u32::MAX);
                            tmp[idx].uid      = euid;
                            tmp[idx].gid      = egid;
                            tmp_set_path(&mut tmp[idx], path);
                            VnodeKind::TmpFile { idx, pos: 0, writable: true }
                        }
                        None => return err_reply(-28), // ENOSPC
                    }
                }
                None => return err_reply(-2), // ENOENT
            }
            }
        } else if lookup_path.starts_with(b"/proc/self/") && lookup_path != b"/proc/self/" {
            let kind = gen_proc_self(pid, lookup_path);
            match kind {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/meminfo" || lookup_path == b"/proc/uptime"
               || lookup_path == b"/proc/loadavg" || lookup_path == b"/proc/stat"
               || lookup_path == b"/proc/self" || lookup_path == b"/proc/mounts" {
            match gen_proc_system(lookup_path) {
                Some(v) => v,
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
            let mut found = {
                let devices = DYNAMIC_DEVICES.lock();
                devices.iter()
                    .find(|d| d.in_use && d.path.as_bytes() == lookup_path)
                    .map(|d| VnodeKind::DynamicDevice { port: d.port, dev_id: d.dev_id })
            };
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
            if found.is_none() && is_tmp_path(path) {
                found = Some(VnodeKind::RamFile { data: b"/tmp", pos: 0, is_dir: true });
            }
            // Check tmpfs dirs
            if found.is_none() && is_tmp_path(path) {
                let tmp = TMP_FILES.lock();
                if let Some(_idx) = tmp.iter().position(|e| {
                    e.in_use && e.is_dir && e.path_len == path.len() && &e.path[..path.len()] == path
                }) {
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
                    proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
                    proxy.data[8..16].copy_from_slice(&(flags as u64).to_le_bytes());
                    // Forward the real creation mode. This used to be a
                    // hardcoded 0, which was invisible only because the f2fs
                    // server ignored the field and created everything 0644.
                    proxy.data[16..24].copy_from_slice(&(mode as u64).to_le_bytes());
                    let reply = call_port(port, proxy);
                    let file_id_raw = i64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8]));
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
            proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(flags as u64).to_le_bytes());
            // See above: forward the real mode, not 0.
            proxy.data[16..24].copy_from_slice(&(mode as u64).to_le_bytes());
            let reply = call_port(port, proxy);
            let file_id_raw = i64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8]));
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

    let mut tbls = FD_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) {
        Some(t) => t,
        None    => return err_reply(-12),
    };
    let fd = match tbl.alloc_fd() { Some(f) => f, None => return err_reply(-24) };
    tbl.fds[fd] = FdEntry { kind, flags, in_use: true };
    val_reply(fd as u64)
}

fn handle_read(pid: u32, fd: usize, buf_ptr: usize, count: usize) -> Message {
    if count == 0 { return val_reply(0); }
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    // An O_PATH descriptor carries no access rights — POSIX/Linux answer EBADF.
    if tbl.fds[fd].flags & O_PATH != 0 { return err_reply(-9); }
    let buf = buf_ptr as *mut u8;
    match &mut tbl.fds[fd].kind {
        VnodeKind::DevNull =>
            val_reply(0),
        VnodeKind::DevZero => {
            let n = count.min(4096);
            unsafe { buf.write_bytes(0, n); }
            val_reply(n as u64)
        }
        VnodeKind::DevUrandom => {
            let n = count.min(4096);
            for i in 0..n { unsafe { *buf.add(i) = lfsr_next(); } }
            val_reply(n as u64)
        }
        VnodeKind::DevStdio { target_fd } => {
            let tfd = *target_fd;
            // Same console/recursion guard as the write arm.
            let target_is_proxy = tfd < MAX_FDS && tbl.fds[tfd].in_use
                && matches!(tbl.fds[tfd].kind, VnodeKind::DevStdio { .. });
            let target_tracked = tfd < MAX_FDS && tbl.fds[tfd].in_use;
            drop(tbls);
            if !target_tracked || target_is_proxy { return err_reply(-9); }
            // Re-enter as read on the target fd.
            handle_read(pid, tfd, buf_ptr, count)
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
            unsafe {
                core::ptr::copy_nonoverlapping(fb_virt as *const u8, buf, n);
            }
            *pos = cur + n;
            val_reply(n as u64)
        }
        VnodeKind::DynamicDevice { port, dev_id } => {
            let port = *port;
            let dev_id = *dev_id;
            drop(tbls);
            let mut proxy_msg = Message::empty();
            proxy_msg.tag = VFS_READ;
            proxy_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
            proxy_msg.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            proxy_msg.data[16..24].copy_from_slice(&(count as u64).to_le_bytes());
            proxy_msg.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes());
            match call_port(port, proxy_msg) {
                reply => reply,
            }
        }
        VnodeKind::RamFile { data, pos, is_dir } => {
            // read(2) on a directory is EISDIR. For a RAMFS_DIRS entry `data`
            // is the directory's own path, not file content — returning it
            // here leaked that static buffer to userspace as file data.
            if *is_dir { return err_reply(-21); } // EISDIR
            let remaining = data.len().saturating_sub(*pos);
            let n = count.min(remaining);
            if n == 0 { return val_reply(0); }
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr().add(*pos), buf, n); }
            *pos += n;
            val_reply(n as u64)
        }
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
            let mut n = 0usize;
            while n < count.min(4096) {
                match r.get() { Some(b) => { unsafe { *buf.add(n) = b; } n += 1; } None => break }
            }
            // Draining bytes frees ring space → a new POLLOUT edge for the
            // write end. Advance the seq so an epoll writer blocked on a full
            // pipe is re-woken edge-triggered.
            if n > 0 { r.seq = r.seq.wrapping_add(1); }
            val_reply(n as u64)
        }
        VnodeKind::TmpFile { idx, pos, .. } => {
            let idx = *idx;
            let cur = *pos;
            drop(tbls);
            let tmp = TMP_FILES.lock();
            // `entry.len` mirrors `vmo.len` for a promoted file, so the EOF
            // bound is the same whether or not a VMO backs this inode.
            let remaining = tmp[idx].len.saturating_sub(cur);
            let n = count.min(remaining).min(4096);
            if n == 0 { return val_reply(0); }
            // Promoted (memfd / MAP_SHARED-mapped) files read from their VMO
            // frames — the pages ARE the file, so read()↔mmap coherence is free.
            let vmos = TMP_VMOS.lock();
            if let Some(vmo) = vmos[idx].as_ref() {
                unsafe { vmo_copy_out(vmo, cur, buf, n); }
            } else {
                unsafe { core::ptr::copy_nonoverlapping(tmp[idx].data.as_ptr().add(cur), buf, n); }
            }
            drop(vmos);
            drop(tmp);
            let mut tbls2 = FD_TABLES.lock();
            if let Some(tbl2) = find_tbl(pid, &mut *tbls2) {
                if fd < MAX_FDS {
                    if let VnodeKind::TmpFile { pos: p, .. } = &mut tbl2.fds[fd].kind { *p = cur + n; }
                }
            }
            val_reply(n as u64)
        }
        VnodeKind::EventFd { slot } => {
            let slot = *slot;
            drop(tbls);
            if count < 8 { return err_reply(-22); } // EINVAL
            let mut counters = EVENTFD_COUNTERS.lock();
            let val = counters[slot];
            if val == 0 { return err_reply(-11); } // EAGAIN
            counters[slot] = 0;
            drop(counters);
            unsafe { (buf as *mut u64).write(val); }
            val_reply(8)
        }
        VnodeKind::TimerFd { slot } => {
            let slot = *slot;
            drop(tbls);
            if count < 8 { return err_reply(-22); } // EINVAL
            let exp = timerfd_poll_expirations(slot);
            if exp == 0 { return err_reply(-11); } // EAGAIN
            TIMERFD_POOL.lock()[slot].expirations = 0;
            unsafe { (buf as *mut u64).write(exp); }
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
        _ => err_reply(-9),
    }
}

fn handle_write(pid: u32, fd: usize, buf_ptr: usize, count: usize) -> Message {
    if count == 0 { return val_reply(0); }
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    // See handle_read: O_PATH grants no access rights.
    if tbl.fds[fd].flags & O_PATH != 0 { return err_reply(-9); }
    let buf = buf_ptr as *const u8;
    match &mut tbl.fds[fd].kind {
        VnodeKind::DevUrandom | VnodeKind::DevNull | VnodeKind::DevZero =>
            val_reply(count as u64),
        VnodeKind::Pipe { ring, is_write: true } => {
            let ring_idx = *ring;
            drop(tbls);
            let mut rings = PIPE_RINGS.lock();
            let r = &mut rings[ring_idx];
            if r.readers == 0 { return err_reply(-32); } // EPIPE
            let mut n = 0usize;
            while n < count {
                if !r.put(unsafe { *buf.add(n) }) { break; }
                n += 1;
            }
            if n > 0 { r.seq = r.seq.wrapping_add(1); } // new readable edge for the read end
            // A full ring must report EAGAIN, never a zero-length write: Rust's
            // `write_all` maps Ok(0) to ErrorKind::WriteZero and gives up, so a
            // pipeline moving more than PIPE_RING_SIZE bytes failed outright
            // instead of applying backpressure. The kernel's sys_write blocks
            // and retries on EAGAIN for blocking fds (mirroring sys_read).
            if n == 0 && count > 0 { return err_reply(-11); } // EAGAIN
            val_reply(n as u64)
        }
        VnodeKind::TmpFile { idx, pos, writable } => {
            if !*writable { return err_reply(-9); } // not open for writing
            let idx = *idx;
            let append = tbl.fds[fd].flags & O_APPEND != 0;
            let cur = if append {
                drop(tbls);
                TMP_FILES.lock()[idx].len
            } else {
                let c = *pos;
                drop(tbls);
                c
            };
            let mut tmp = TMP_FILES.lock();
            let mut vmos = TMP_VMOS.lock();
            let (n, new_pos) = if let Some(vmo) = vmos[idx].as_mut() {
                // Promoted file: write into VMO frames, no 32 KiB cap. Grow the
                // frame list to cover cur+count first (F_SEAL_WRITE/GROW are
                // out of scope — not enforced here).
                let end = cur + count;
                let need_pages = (end + 4095) / 4096;
                while vmo.pages.len() < need_pages {
                    match vmo_alloc_zeroed_frame() { Some(f) => vmo.pages.push(f), None => break }
                }
                let cap_bytes = vmo.pages.len() * 4096;
                let n = count.min(cap_bytes.saturating_sub(cur));
                if n == 0 { return err_reply(-28); } // ENOSPC
                unsafe { vmo_copy_in(vmo, cur, buf, n); }
                let new_pos = cur + n;
                if new_pos > vmo.len { vmo.len = new_pos; tmp[idx].len = new_pos; } // mirror EOF
                (n, new_pos)
            } else {
                let entry = &mut tmp[idx];
                let avail = MAX_TMP_SIZE.saturating_sub(cur);
                let n = count.min(avail);
                if n == 0 { return err_reply(-28); } // ENOSPC
                unsafe { core::ptr::copy_nonoverlapping(buf, entry.data.as_mut_ptr().add(cur), n); }
                let new_pos = cur + n;
                if new_pos > entry.len { entry.len = new_pos; }
                (n, new_pos)
            };
            drop(vmos);
            drop(tmp);
            let mut tbls2 = FD_TABLES.lock();
            if let Some(tbl2) = find_tbl(pid, &mut *tbls2) {
                if fd < MAX_FDS {
                    if let VnodeKind::TmpFile { pos: p, .. } = &mut tbl2.fds[fd].kind { *p = new_pos; }
                }
            }
            val_reply(n as u64)
        }
        VnodeKind::EventFd { slot } => {
            let slot = *slot;
            drop(tbls);
            if count < 8 { return err_reply(-22); } // EINVAL
            let addval = unsafe { (buf as *const u64).read() };
            if addval == u64::MAX { return err_reply(-22); } // EINVAL
            let mut counters = EVENTFD_COUNTERS.lock();
            counters[slot] = counters[slot].saturating_add(addval);
            drop(counters);
            let mut seqs = EVENTFD_SEQ.lock();
            seqs[slot] = seqs[slot].wrapping_add(1);
            val_reply(8)
        }
        VnodeKind::DevStdio { target_fd } => {
            let tfd = *target_fd;
            // Console targets are served by the kernel's serial fast path
            // (sys_write consults fd_is_console_stdio before routing here);
            // recursing into another DevStdio would loop forever.
            let target_is_proxy = tfd < MAX_FDS && tbl.fds[tfd].in_use
                && matches!(tbl.fds[tfd].kind, VnodeKind::DevStdio { .. });
            let target_tracked = tfd < MAX_FDS && tbl.fds[tfd].in_use;
            drop(tbls);
            if !target_tracked || target_is_proxy { return err_reply(-9); }
            handle_write(pid, tfd, buf_ptr, count)
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
            
            // If VirtIO-GPU is present, trigger a flush for the console resource (1)
            extern "C" { fn fb_flush(); }
            unsafe { fb_flush(); }
            
            val_reply(n as u64)
        }
        VnodeKind::DynamicDevice { port, dev_id } => {
            let port = *port;
            let dev_id = *dev_id;
            drop(tbls);
            let mut proxy_msg = Message::empty();
            proxy_msg.tag = VFS_WRITE;
            proxy_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
            proxy_msg.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            proxy_msg.data[16..24].copy_from_slice(&(count as u64).to_le_bytes());
            proxy_msg.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes());
            match call_port(port, proxy_msg) {
                reply => reply,
            }
        }
        VnodeKind::MountedFile { port, file_id } => {
            let port = *port; let file_id = *file_id;
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_WRITE;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            proxy.data[16..24].copy_from_slice(&(count as u64).to_le_bytes());
            call_port(port, proxy)
        }
        _ => err_reply(-9),
    }
}

fn handle_close(pid: u32, fd: usize) -> Message {
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    
    let kind = tbl.fds[fd].kind;
    tbl.fds[fd] = FdEntry::empty();
    drop(tbls);

    if let Some(key) = lock_key_of(&kind) { release_locks(key, pid); }

    match kind {
        VnodeKind::Pipe { ring, is_write } => {
            pipe_drop_ref(&mut PIPE_RINGS.lock(), ring, is_write);
        }
        VnodeKind::EventFd { slot } => {
            EVENTFD_COUNTERS.lock()[slot] = u64::MAX;
        }
        VnodeKind::TimerFd { slot } => {
            TIMERFD_POOL.lock()[slot] = TimerFdEntry::free();
        }
        VnodeKind::DynamicDevice { port, dev_id } => {
            let mut close_msg = Message::empty();
            close_msg.tag = VFS_CLOSE;
            close_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
            let _ = call_port(port, close_msg);
        }
        VnodeKind::MountedFile { port, file_id } => {
            // A dup'd fd shares this (port, file_id), so the mount server's
            // open-file slot must survive until the *last* of them is closed.
            // The just-closed fd was already cleared above, so this scan sees
            // only the survivors. Without it, closing a dup — e.g. the throwaway
            // fd `fdopendir` makes to run one `readdir` — freed the slot out from
            // under the original, and the next fstat/unlink on it came back
            // EBADF. That is what broke `rm -r`, `du` and every fts-style walk.
            let still_referenced = {
                let tbls = FD_TABLES.lock();
                tbls.iter().any(|t| t.in_use && t.fds.iter().any(|f| {
                    f.in_use && matches!(f.kind,
                        VnodeKind::MountedFile { port: p, file_id: i } if p == port && i == file_id)
                }))
            };
            if !still_referenced {
                let mut proxy = Message::empty();
                proxy.tag = VFS_CLOSE;
                proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
                let _ = call_port(port, proxy);
            }
        }
        VnodeKind::TmpFile { idx, .. } => tmp_release_ephemeral(idx),
        _ => {}
    }
    ok_reply()
}

fn handle_lseek(pid: u32, fd: usize, offset: i64, whence: u32) -> Message {
    const SEEK_SET: u32 = 0;
    const SEEK_CUR: u32 = 1;
    const SEEK_END: u32 = 2;
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    match &mut tbl.fds[fd].kind {
        VnodeKind::RamFile { data, pos, .. } => {
            // For a pseudo-directory `pos` is the getdents64 cursor, so
            // lseek(fd, 0, SEEK_SET) still works as rewinddir().
            let len = data.len() as i64;
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
        VnodeKind::TmpFile { idx, pos, .. } => {
            let idx = *idx;
            let cur = *pos as i64;
            let tmp = TMP_FILES.lock();
            let file_len = tmp[idx].len as i64;
            drop(tmp);
            let new_pos = match whence {
                SEEK_SET => offset,
                SEEK_CUR => cur + offset,
                SEEK_END => file_len + offset,
                _        => return err_reply(-22),
            };
            if new_pos < 0 { return err_reply(-22); }
            *pos = new_pos as usize;
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
        _ => err_reply(-29), // ESPIPE — not seekable (pipes, devnull, etc.)
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
        let mut rings = PIPE_RINGS.lock();
        let mut found = None;
        for (i, r) in rings.iter().enumerate() {
            if r.readers == 0 && r.writers == 0 && r.count == 0 {
                found = Some(i); break;
            }
        }
        let i = match found { Some(i) => i, None => return err_reply(-23) };
        rings[i].readers = 1;
        rings[i].writers = 1;
        i
    };
    let mut tbls = FD_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-12) };
    let rfd = match tbl.alloc_fd() { Some(f) => f, None => return err_reply(-24) };
    tbl.fds[rfd] = FdEntry { kind: VnodeKind::Pipe { ring: ring_idx, is_write: false },
                             flags: inherited, in_use: true };
    let wfd = match tbl.alloc_fd() { Some(f) => f, None => {
        tbl.fds[rfd] = FdEntry::empty(); return err_reply(-24);
    }};
    tbl.fds[wfd] = FdEntry { kind: VnodeKind::Pipe { ring: ring_idx, is_write: true },
                             flags: inherited | O_WRONLY, in_use: true };
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
pub fn fd_redirected(pid: u32, fd: usize) -> bool {
    let pid = sched::tgid_of(pid); // fd tables are per-process
    if fd >= MAX_FDS { return false; }
    let mut tbls = FD_TABLES.lock();
    match find_tbl(pid, &mut *tbls) {
        Some(t) => t.fds[fd].in_use,
        None    => false,
    }
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
    if oldfd >= MAX_FDS || newfd >= MAX_FDS { return err_reply(-9); }
    let mut tbls = FD_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds[oldfd].in_use {
        // Untracked fd 0-2 = raw console — same implicit /dev/stdio proxy
        // rule as handle_alloc_fd (e.g. dup2(dup-of-stdout, 1) round trips).
        if oldfd <= 2 && oldfd != newfd {
            let replaced = if tbl.fds[newfd].in_use { Some(tbl.fds[newfd].kind) } else { None };
            tbl.fds[newfd] = FdEntry {
                kind:   VnodeKind::DevStdio { target_fd: oldfd },
                flags:  if cloexec { O_CLOEXEC } else { 0 },
                in_use: true,
            };
            drop(tbls);
            if let Some(old) = replaced { pipe_ref_dec(&old); }
            return val_reply(newfd as u64);
        }
        if oldfd <= 2 && oldfd == newfd { return val_reply(newfd as u64); }
        return err_reply(-9);
    }
    if oldfd == newfd { return val_reply(newfd as u64); } // dup2(fd, fd) is a no-op
    // dup2 silently closes newfd first if it was open; drop its pipe ref.
    let replaced = if tbl.fds[newfd].in_use { Some(tbl.fds[newfd].kind) } else { None };
    let dupled = tbl.fds[oldfd].kind;
    tbl.fds[newfd] = tbl.fds[oldfd];
    // The duplicate never inherits close-on-exec — only dup3's own flag sets it.
    if cloexec { tbl.fds[newfd].flags |= O_CLOEXEC; }
    else       { tbl.fds[newfd].flags &= !O_CLOEXEC; }
    drop(tbls);
    if let Some(old) = replaced { pipe_ref_dec(&old); }
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
}

/// Lift `fd` out of `pid`'s table into a `TransferFd`, taking an in-flight
/// reference on the underlying object. Returns None (→ EBADF) for a closed or
/// out-of-range fd, or for the untracked console fds 0-2 (passing stdio over
/// SCM_RIGHTS is not needed by Wayland/D-Bus; see the K1 report).
pub fn export_fd(pid: u32, fd: usize) -> Option<TransferFd> {
    let (kind, flags) = {
        let tbls = FD_TABLES.lock();
        let tbl = tbls.iter().find(|t| t.in_use && t.pid == pid)?;
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return None; }
        (tbl.fds[fd].kind, tbl.fds[fd].flags)
    };
    // Second reference held by the queued descriptor: a pipe endpoint must not
    // reach EOF/EPIPE just because the sender closed its fd before the peer
    // recv'd. No-op for every non-pipe kind (their lifetime is table-scan
    // driven — see `tmp_release_ephemeral`).
    pipe_ref_inc(&kind);
    Some(TransferFd { kind, flags })
}

/// Install a queued `TransferFd` as a fresh fd in `pid`'s table, consuming the
/// in-flight reference `export_fd` took (the installed fd now owns it — so no
/// extra ref bump). `cloexec` sets FD_CLOEXEC per MSG_CMSG_CLOEXEC. Returns the
/// new fd, or -EMFILE if the table is full (in which case the reference is
/// released, matching a close of the undelivered fd).
pub fn import_fd(pid: u32, tf: TransferFd, cloexec: bool) -> isize {
    let mut tbls = FD_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) { Some(t) => t, None => {
        drop(tbls); release_vnode(tf.kind, pid); return -24;
    }};
    let slot = match tbl.alloc_fd() { Some(s) => s, None => {
        drop(tbls); release_vnode(tf.kind, pid); return -24; // EMFILE
    }};
    let mut flags = tf.flags;
    if cloexec { flags |= O_CLOEXEC; } else { flags &= !O_CLOEXEC; }
    tbl.fds[slot] = FdEntry { kind: tf.kind, flags, in_use: true };
    slot as isize
}

/// Release a `TransferFd` that never reached a receiver — the in-flight
/// reference `export_fd` took is dropped, and any last-reference teardown runs
/// exactly as a close would. Linux closes SCM_RIGHTS fds that don't fit the
/// receiver's control buffer, and drops queued fds when the socket dies.
pub fn drop_transfer(tf: TransferFd) {
    release_vnode(tf.kind, 0);
}

fn handle_fork_dup(parent_pid: u32, child_pid: u32) -> Message {
    let mut tbls = FD_TABLES.lock();
    let parent_fds: [FdEntry; MAX_FDS] = match tbls.iter().find(|t| t.in_use && t.pid == parent_pid) {
        Some(t) => t.fds,
        None    => return ok_reply(),
    };
    if let Some(slot) = tbls.iter_mut().find(|t| !t.in_use) {
        *slot = ProcFdTable::empty();
        slot.in_use = true;
        slot.pid    = child_pid;
        slot.fds    = parent_fds;
    }
    drop(tbls);
    // The child now holds a second fd on every inherited pipe endpoint, so its
    // reader/writer refcount must go up: otherwise the parent closing its copy
    // would falsely signal EOF/EPIPE/POLLHUP to the still-open child (and vice
    // versa) — the exact defect that broke poll/select/epoll across fork.
    for f in parent_fds.iter() {
        if f.in_use { pipe_ref_inc(&f.kind); }
    }
    ok_reply()
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
            pipe_drop_ref(&mut PIPE_RINGS.lock(), ring, is_write);
        }
        VnodeKind::EventFd { slot } => { EVENTFD_COUNTERS.lock()[slot] = u64::MAX; }
        VnodeKind::TimerFd { slot } => { TIMERFD_POOL.lock()[slot] = TimerFdEntry::free(); }
        VnodeKind::DynamicDevice { port, dev_id } => {
            let mut close_msg = Message::empty();
            close_msg.tag = VFS_CLOSE;
            close_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
            let _ = call_port(port, close_msg);
        }
        VnodeKind::TmpFile { idx, .. } => tmp_release_ephemeral(idx),
        _ => {}
    }
}

fn handle_exec_cloexec(pid: u32) -> Message {
    // Collect first, release after dropping the table lock: release_vnode takes
    // PIPE_RINGS and may call out to a device port.
    let mut closed = [VnodeKind::None; MAX_FDS];
    {
        let mut tbls = FD_TABLES.lock();
        if let Some(t) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
            for (i, fd) in t.fds.iter_mut().enumerate() {
                if fd.in_use && fd.flags & O_CLOEXEC != 0 {
                    closed[i] = fd.kind;
                    *fd = FdEntry::empty();
                }
            }
        }
    }
    // Without this the close-on-exec sweep silently leaked a reference on every
    // pipe end it retired, so the peer's reader never reached EOF: brush's
    // `$(...)` read_to_string and every `a | b` reader blocked forever waiting
    // for a writer count that could no longer reach zero.
    for kind in closed {
        if !matches!(kind, VnodeKind::None) { release_vnode(kind, pid); }
    }
    ok_reply()
}

fn handle_close_all(pid: u32) -> Message {
    let mut tbls = FD_TABLES.lock();
    if let Some(t) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        // Collect active FDs to close
        let mut fds_to_close = [VnodeKind::None; MAX_FDS];
        for i in 0..MAX_FDS {
            if t.fds[i].in_use {
                fds_to_close[i] = t.fds[i].kind;
            }
        }
        *t = ProcFdTable::empty();
        drop(tbls);
        
        // Close them all properly
        for kind in fds_to_close {
            release_vnode(kind, pid);
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

const MAX_LOCKS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LockKey { Tmp(usize), Mount(u32, u32) }

#[derive(Clone, Copy)]
struct LockRecord {
    key:       LockKey,
    pid:       u32,
    start:     u64,
    end:       u64, // exclusive upper bound; u64::MAX == to EOF
    exclusive: bool,
    in_use:    bool,
}

impl LockRecord {
    const fn empty() -> Self {
        Self { key: LockKey::Tmp(0), pid: 0, start: 0, end: 0, exclusive: false, in_use: false }
    }
}

static LOCKS: Mutex<[LockRecord; MAX_LOCKS]> = Mutex::new([const { LockRecord::empty() }; MAX_LOCKS]);

/// Identify the lock domain of a vnode. Only regular tmpfs/mounted files are lockable.
fn lock_key_of(kind: &VnodeKind) -> Option<LockKey> {
    match kind {
        VnodeKind::TmpFile { idx, .. }           => Some(LockKey::Tmp(*idx)),
        VnodeKind::MountedFile { port, file_id }  => Some(LockKey::Mount(*port, *file_id)),
        _ => None,
    }
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

/// Release every lock `pid` holds on `key`.
fn release_locks(key: LockKey, pid: u32) {
    let mut locks = LOCKS.lock();
    for l in locks.iter_mut() {
        if l.in_use && l.key == key && l.pid == pid { *l = LockRecord::empty(); }
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
        SEEK_CUR => match kind { VnodeKind::TmpFile { pos, .. } => *pos as i64, _ => return None },
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
    sched::with_current_address_space(|as_| as_.write_user_buf(ptr, &buf)).unwrap_or(false)
}

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

fn handle_fcntl_lock(pid: u32, kind: VnodeKind, cmd: usize, arg: usize) -> Message {
    const F_GETLK:  usize = 5;
    const F_SETLK:  usize = 6;

    let key = match lock_key_of(&kind) { Some(k) => k, None => return err_reply(-22) }; // EINVAL
    let (l_type, l_whence, l_start, l_len, _) = match read_flock(arg) {
        Some(v) => v, None => return err_reply(-14), // EFAULT
    };
    let (start, end) = match resolve_lock_range(&kind, l_whence, l_start, l_len) {
        Some(r) => r, None => return err_reply(-22),
    };

    if l_type == F_UNLCK {
        let mut locks = LOCKS.lock();
        for l in locks.iter_mut() {
            if l.in_use && l.key == key && l.pid == pid && ranges_overlap(l.start, l.end, start, end) {
                *l = LockRecord::empty();
            }
        }
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
    loop {
        let mut locks = LOCKS.lock();
        if find_conflict(&*locks, key, pid, start, end, exclusive).is_none() {
            // No sub-range splitting: a new grant simply supersedes any range
            // this same pid already held that overlaps it.
            for l in locks.iter_mut() {
                if l.in_use && l.key == key && l.pid == pid && ranges_overlap(l.start, l.end, start, end) {
                    *l = LockRecord::empty();
                }
            }
            return match locks.iter_mut().find(|l| !l.in_use) {
                Some(slot) => { *slot = LockRecord { key, pid, start, end, exclusive, in_use: true }; ok_reply() }
                None => err_reply(-37), // ENOLCK
            };
        }
        drop(locks);
        if cmd == F_SETLK { return err_reply(-11); } // EAGAIN
        sched::yield_now("fcntl_setlkw");
    }
}

fn handle_flock(pid: u32, fd: usize, op: u32) -> Message {
    const LOCK_SH: u32 = 1;
    const LOCK_EX: u32 = 2;
    const LOCK_NB: u32 = 4;
    const LOCK_UN: u32 = 8;

    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    let key = match lock_key_of(&tbl.fds[fd].kind) { Some(k) => k, None => return err_reply(-22) };
    drop(tbls);

    if op & LOCK_UN != 0 {
        release_locks(key, pid);
        return ok_reply();
    }

    let exclusive = op & LOCK_EX != 0;
    if !exclusive && op & LOCK_SH == 0 { return err_reply(-22); } // EINVAL
    let nonblock = op & LOCK_NB != 0;

    loop {
        let mut locks = LOCKS.lock();
        if find_conflict(&*locks, key, pid, 0, u64::MAX, exclusive).is_none() {
            for l in locks.iter_mut() {
                if l.in_use && l.key == key && l.pid == pid { *l = LockRecord::empty(); }
            }
            return match locks.iter_mut().find(|l| !l.in_use) {
                Some(slot) => {
                    *slot = LockRecord { key, pid, start: 0, end: u64::MAX, exclusive, in_use: true };
                    ok_reply()
                }
                None => err_reply(-37), // ENOLCK
            };
        }
        drop(locks);
        if nonblock { return err_reply(-11); } // EWOULDBLOCK
        sched::yield_now("flock");
    }
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

    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
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
        F_SETFL => { tbl.fds[fd].flags = (tbl.fds[fd].flags & O_CLOEXEC) | arg as u32; ok_reply() }
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
                    Some(vmo) if vmo.is_memfd => { vmo.seals |= arg as u32; ok_reply() }
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
    if oldfd >= MAX_FDS || minfd >= MAX_FDS { return err_reply(-9); }
    // fds 0-2 are never handed out as dup targets: the kernel's read/write
    // fast paths hardwire them to the console (see ProcFdTable::alloc_fd).
    let floor = minfd.max(3);
    let mut tbls = FD_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-12) };

    let newfd = match tbl.fds.iter().enumerate()
                    .find(|(i, f)| *i >= floor && *i != oldfd && !f.in_use)
                    .map(|(i, _)| i) {
        Some(f) => f, None => return err_reply(-24) // EMFILE
    };

    if !tbl.fds[oldfd].in_use {
        // An untracked fd 0-2 is the raw console: dup() of it yields a
        // /dev/stdin|stdout|stderr proxy, exactly what opening those paths
        // creates. (std's try_clone_to_owned on stdio — fcntl F_DUPFD — and
        // command_fds' stdio mappings depend on this producing a real fd.)
        if oldfd <= 2 {
            tbl.fds[newfd] = FdEntry {
                kind:   VnodeKind::DevStdio { target_fd: oldfd },
                flags:  if cloexec { O_CLOEXEC } else { 0 },
                in_use: true,
            };
            return val_reply(newfd as u64);
        }
        return err_reply(-9);
    }

    tbl.fds[newfd] = tbl.fds[oldfd];
    tbl.fds[newfd].flags = if cloexec {
        tbl.fds[oldfd].flags | O_CLOEXEC
    } else {
        tbl.fds[oldfd].flags & !O_CLOEXEC
    };
    let dupled = tbl.fds[newfd].kind;
    drop(tbls);
    pipe_ref_inc(&dupled); // newfd is a second fd on the same pipe endpoint
    val_reply(newfd as u64)
}

/// Allocate a new fd number pointing at the same vnode as `oldfd`.
/// Used by sys_dup() which doesn't know the new fd number in advance.
fn handle_alloc_fd(pid: u32, oldfd: usize) -> Message {
    dup_fd_min(pid, oldfd, 3, false)
}

/// Store the getdents64 cursor back into a directory fd without changing what
/// kind of directory it is. A tmpfs directory is a `TmpFile` vnode whose pool
/// slot carries `is_dir`; overwriting it with a `RamFile` (as the enumeration
/// used to do unconditionally) would detach it from its pool entry.
fn set_dir_pos(kind: &mut VnodeKind, new_pos: usize) {
    match kind {
        VnodeKind::RamFile { pos, .. } => *pos = new_pos,
        VnodeKind::TmpFile { pos, .. }  => *pos = new_pos,
        _ => {}
    }
}

/// getdents64 — fill `buf` with `struct linux_dirent64` entries for `fd`.
fn handle_getdents64(pid: u32, fd: usize, buf_ptr: usize, count: usize) -> Message {
    if count < 64 { return err_reply(-22); }

    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }

    // A tmpfs directory's path is not `'static` (it lives in the TMP_FILES
    // pool and can be rmdir'd), so copy it into a frame-local buffer and
    // borrow *that* for the whole enumeration.
    let mut tmp_dir_buf = [0u8; MAX_TMP_PATH];
    let mut tmp_dir_len = 0usize;
    let mut dir_is_tmp  = false;
    let (static_path, start_pos): (&'static [u8], usize) = match &tbl.fds[fd].kind {
        VnodeKind::RamFile { data, pos, .. } => (*data, *pos),
        VnodeKind::TmpFile { idx, pos, .. } => {
            let i = *idx; let p = *pos;
            let t = TMP_FILES.lock();
            if !t[i].in_use || !t[i].is_dir { return err_reply(-20); } // ENOTDIR
            tmp_dir_len = t[i].path_len;
            tmp_dir_buf[..tmp_dir_len].copy_from_slice(&t[i].path[..tmp_dir_len]);
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
        else { set_dir_pos(&mut tbl.fds[fd].kind, pos); return val_reply(off as u64); }
    }

    let mut virtual_idx = 2usize;

    // RAMFS directories
    for &child_dir in RAMFS_DIRS {
        if child_dir == dir_path { continue; }
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
                    set_dir_pos(&mut tbl.fds[fd].kind, pos);
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
                    set_dir_pos(&mut tbl.fds[fd].kind, pos);
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
    {
        let tmp = TMP_FILES.lock();
        for (i, e) in tmp.iter().enumerate() {
            // Ephemeral /proc snapshots park under fake "/tmp/.proc_N" paths;
            // they are fd-owned scratch, not directory contents.
            if !e.in_use || e.ephemeral { continue; }
            let p = &e.path[..e.path_len];
            // Direct child of dir_path? (tmpfs paths are always absolute and
            // rooted at /tmp, so dir_path is never "/" here.)
            if p.len() <= dir_len + 1 || !p.starts_with(dir_path) || p[dir_len] != b'/' { continue; }
            let name = &p[dir_len + 1..];
            if name.is_empty() || name.contains(&b'/') { continue; }
            if virtual_idx >= pos {
                // DT_DIR / DT_FIFO / DT_REG
                // DT_DIR / DT_FIFO / DT_LNK / DT_REG. `ls -F` prints the '@'
                // suffix and `ls -l` the 'l' type char straight off this.
                let d_type = if e.is_dir { 4 }
                             else if e.is_fifo { 1 }
                             else if e.is_link { 10 }
                             else if e.is_sock { 12 } // DT_SOCK
                             else { 8 };
                if let Some(r) = write_dirent(buf, off, count, 400 + i as u64, name, d_type) {
                    off += r; pos += 1;
                } else {
                    drop(tmp);
                    set_dir_pos(&mut tbl.fds[fd].kind, pos);
                    return val_reply(off as u64);
                }
            }
            virtual_idx += 1;
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
                        
                        // Check if we already added this directory
                        if !seen_dirs.iter().any(|&d| d == Some(dir_name)) {
                            if virtual_idx >= pos {
                                if let Some(r) = write_dirent(buf, off, count, virtual_idx as u64 + 300, dir_name.as_bytes(), 4) { // 4 = DT_DIR
                                    off += r; pos += 1;
                                } else {
                                    drop(devices);
                                    set_dir_pos(&mut tbl.fds[fd].kind, pos);
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
                            if let Some(r) = write_dirent(buf, off, count, virtual_idx as u64 + 300, name.as_bytes(), 8) { // 8 = DT_REG
                                off += r; pos += 1;
                            } else {
                                drop(devices);
                                set_dir_pos(&mut tbl.fds[fd].kind, pos);
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
                            set_dir_pos(&mut tbl.fds[fd].kind, pos);
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

    set_dir_pos(&mut tbl.fds[fd].kind, pos);
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

fn find_tbl<'a>(pid: u32, tbls: &'a mut [ProcFdTable]) -> Option<&'a mut ProcFdTable> {
    tbls.iter_mut().find(|t| t.in_use && t.pid == pid)
}

fn get_or_create<'a>(pid: u32, tbls: &'a mut [ProcFdTable]) -> Option<&'a mut ProcFdTable> {
    if let Some(pos) = tbls.iter().position(|t| t.in_use && t.pid == pid) { return Some(&mut tbls[pos]); }
    if let Some(pos) = tbls.iter().position(|t| !t.in_use) {
        tbls[pos] = ProcFdTable::empty();
        tbls[pos].in_use = true;
        tbls[pos].pid    = pid;
        return Some(&mut tbls[pos]);
    }
    None
}

fn read_cstr_raw(ptr: usize) -> Option<([u8; 256], usize)> {
    if ptr == 0 { return None; }
    let mut buf = [0u8; 256];
    for (i, slot) in buf.iter_mut().enumerate() {
        let b = unsafe { *(ptr as *const u8).add(i) };
        if b == 0 { return Some((buf, i)); }
        *slot = b;
    }
    None
}

fn path_eq(buf: &[u8; 256], len: usize, path: &[u8]) -> bool {
    len == path.len() && buf[..len] == *path
}

static _SERVER_PORT_ID: atomic::AtomicU32 = atomic::AtomicU32::new(u32::MAX);

fn handle_eventfd(pid: u32, initval: u64) -> Message {
    let mut counters = EVENTFD_COUNTERS.lock();
    let slot = match counters.iter().position(|&v| v == u64::MAX) {
        Some(s) => s, None => return err_reply(-24),
    };
    counters[slot] = if initval == u64::MAX { u64::MAX - 1 } else { initval };
    drop(counters);
    let mut tbls = FD_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) {
        Some(t) => t, None => { EVENTFD_COUNTERS.lock()[slot] = u64::MAX; return err_reply(-24); }
    };
    let fd = match tbl.alloc_fd() {
        Some(f) => f, None => { EVENTFD_COUNTERS.lock()[slot] = u64::MAX; return err_reply(-24); }
    };
    tbl.fds[fd] = FdEntry { kind: VnodeKind::EventFd { slot }, flags: 0, in_use: true };
    val_reply(fd as u64)
}

fn handle_timerfd_create(pid: u32) -> Message {
    let mut pool = TIMERFD_POOL.lock();
    let slot = match pool.iter().position(|e| e.is_free()) {
        Some(s) => s, None => return err_reply(-24),
    };
    pool[slot] = TimerFdEntry::free();
    pool[slot].deadline_ticks = 1;
    drop(pool);
    let mut tbls = FD_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) {
        Some(t) => t, None => { TIMERFD_POOL.lock()[slot] = TimerFdEntry::free(); return err_reply(-24); }
    };
    let fd = match tbl.alloc_fd() {
        Some(f) => f, None => { TIMERFD_POOL.lock()[slot] = TimerFdEntry::free(); return err_reply(-24); }
    };
    tbl.fds[fd] = FdEntry { kind: VnodeKind::TimerFd { slot }, flags: 0, in_use: true };
    val_reply(fd as u64)
}

fn handle_timerfd_settime(pid: u32, fd: usize, value_ns: u64, interval_ns: u64) -> Message {
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    let slot = match tbl.fds[fd].kind { VnodeKind::TimerFd { slot } => slot, _ => return err_reply(-22) };
    drop(tbls);
    const NS_PER_TICK: u64 = 10_000_000;
    let now = sched::ticks();
    let mut pool = TIMERFD_POOL.lock();
    let e = &mut pool[slot];
    if value_ns == 0 { e.armed = false; e.expirations = 0; }
    else { e.armed = true; e.deadline_ticks = now + (value_ns / NS_PER_TICK).max(1); e.interval_ticks = interval_ns / NS_PER_TICK; e.expirations = 0; }
    ok_reply()
}

fn handle_timerfd_gettime(pid: u32, fd: usize, out_ptr: usize) -> Message {
    if out_ptr == 0 { return err_reply(-14); }
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    let slot = match tbl.fds[fd].kind { VnodeKind::TimerFd { slot } => slot, _ => return err_reply(-22) };
    drop(tbls);
    const NS_PER_TICK: u64 = 10_000_000;
    let pool = TIMERFD_POOL.lock();
    let e = &pool[slot];
    let now = sched::ticks();
    let remaining_ns = if e.armed && e.deadline_ticks > now { (e.deadline_ticks - now) * NS_PER_TICK } else { 0 };
    let interval_ns = e.interval_ticks * NS_PER_TICK;
    drop(pool);
    unsafe {
        let p = out_ptr as *mut i64;
        p.write((interval_ns / 1_000_000_000) as i64);
        p.add(1).write((interval_ns % 1_000_000_000) as i64);
        p.add(2).write((remaining_ns / 1_000_000_000) as i64);
        p.add(3).write((remaining_ns % 1_000_000_000) as i64);
    }
    ok_reply()
}

const FIONREAD: usize = 0x541B;
const FBIOGET_VSCREENINFO: usize = 0x4600;

fn handle_ioctl(pid: u32, fd: usize, cmd: usize, arg: usize) -> Message {
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }


    if let VnodeKind::DynamicDevice { port, dev_id } = &tbl.fds[fd].kind {
        let port = *port;
        let dev_id = *dev_id;
        drop(tbls);

        let mut proxy_msg = Message::empty();
        proxy_msg.tag = VFS_IOCTL;
        proxy_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
        proxy_msg.data[8..16].copy_from_slice(&(cmd as u64).to_le_bytes());
        proxy_msg.data[16..24].copy_from_slice(&(arg as u64).to_le_bytes());
        proxy_msg.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes());

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

            let ok = sched::with_current_address_space(|as_| {
                unsafe {
                    as_.write_user_buf(arg, core::slice::from_raw_parts(&info as *const _ as *const u8, 32))
                }
            }).unwrap_or(false);

            if !ok { return err_reply(-14); } // EFAULT
            return ok_reply();
        }
    }

    if cmd != FIONREAD { return err_reply(-25); }
    if arg == 0 { return err_reply(-14); }

    let bytes_avail: i32 = match &tbl.fds[fd].kind {
        VnodeKind::Pipe { ring, is_write: false } => { let r = *ring; drop(tbls); PIPE_RINGS.lock()[r].count as i32 }
        VnodeKind::RamFile { data, pos, is_dir } => {
            if *is_dir { return err_reply(-25); } // ENOTTY — no readable byte stream
            (data.len().saturating_sub(*pos)) as i32
        }
        VnodeKind::TmpFile { idx, pos, .. } => { let i = *idx; let c = *pos; drop(tbls); TMP_FILES.lock()[i].len.saturating_sub(c) as i32 }
        VnodeKind::EventFd { slot } => { let s = *slot; drop(tbls); if EVENTFD_COUNTERS.lock()[s] > 0 { 8 } else { 0 } }
        VnodeKind::TimerFd { slot } => { let s = *slot; drop(tbls); if timerfd_poll_expirations(s) > 0 { 8 } else { 0 } }
        _ => return err_reply(-25),
    };
    unsafe { (arg as *mut i32).write(bytes_avail); }
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
fn handle_poll(pid: u32, fd: usize) -> Message {
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }

    // Returns (revents, seq). `seq` is a monotonic per-object event counter the
    // epoll layer uses to emulate edge-triggered delivery (see PipeRing::seq /
    // EVENTFD_SEQ). fd kinds with no re-arming edge source report seq 0, which
    // makes an EPOLLET interest fire exactly once for their (constant)
    // readiness — correct edge behaviour for an always-ready fd.
    let (revents, seq): (u32, u64) = match &tbl.fds[fd].kind {
        VnodeKind::Pipe { ring, is_write: false } => {
            let r = *ring;
            drop(tbls);
            let ring = &PIPE_RINGS.lock()[r];
            let mut ev = 0;
            if ring.count > 0 { ev |= POLLIN; }
            if ring.writers == 0 { ev |= POLLIN | POLLHUP; } // EOF: read() returns 0 without blocking
            (ev, ring.seq)
        }
        VnodeKind::Pipe { ring, is_write: true } => {
            let r = *ring;
            drop(tbls);
            let ring = &PIPE_RINGS.lock()[r];
            let ev = if ring.readers == 0 {
                POLLERR // reader gone: next write() gets EPIPE
            } else if ring.count < PIPE_RING_SIZE {
                POLLOUT
            } else {
                0
            };
            (ev, ring.seq)
        }
        VnodeKind::RamFile { .. } | VnodeKind::TmpFile { .. } | VnodeKind::MountedFile { .. }
        | VnodeKind::DevNull | VnodeKind::DevZero | VnodeKind::DevUrandom | VnodeKind::DevFb { .. } => {
            drop(tbls);
            (POLLIN | POLLOUT, 0) // synchronous, memory- or polled-disk-backed I/O never blocks here
        }
        VnodeKind::EventFd { slot } => {
            let s = *slot;
            drop(tbls);
            let mut ev = POLLOUT; // only EINVAL's on overflow, never actually blocks
            if EVENTFD_COUNTERS.lock()[s] > 0 { ev |= POLLIN; }
            (ev, EVENTFD_SEQ.lock()[s])
        }
        VnodeKind::TimerFd { slot } => {
            let s = *slot;
            drop(tbls);
            // Cumulative expiration count doubles as the edge seq: each new
            // expiration advances it; a read that resets expirations drops
            // revents to 0 so no spurious fire results.
            let exp = timerfd_poll_expirations(s);
            (if exp > 0 { POLLIN } else { 0 }, exp)
        }
        VnodeKind::DevStdio { .. } | VnodeKind::DynamicDevice { .. } | VnodeKind::None => {
            drop(tbls);
            (0, 0)
        }
    };
    poll_reply(revents, seq)
}

fn handle_ftruncate(pid: u32, fd: usize, new_len: usize) -> Message {
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => {
            drop(tbls);
            let mut tmp = TMP_FILES.lock();
            let mut vmos = TMP_VMOS.lock();
            if let Some(vmo) = vmos[idx].as_mut() {
                // Enforce F_SEAL_SHRINK; grow/shrink the frame list. Frames a
                // live mapping still holds survive shrink (unref_or_free), so
                // there is no use-after-free (Linux would SIGBUS — out of scope).
                if new_len < vmo.len && vmo.seals & F_SEAL_SHRINK != 0 {
                    return err_reply(-1); // EPERM
                }
                let old_len   = vmo.len;
                let old_pages = vmo.pages.len();
                let new_pages = (new_len + 4095) / 4096;
                if new_pages >= old_pages {
                    while vmo.pages.len() < new_pages {
                        match vmo_alloc_zeroed_frame() {
                            Some(f) => vmo.pages.push(f),
                            None    => return err_reply(-28), // ENOSPC
                        }
                    }
                    // Clear the tail of the last previously-existing page; newly
                    // appended frames are already zero from allocation.
                    if new_len > old_len {
                        let end = new_len.min(old_pages * 4096);
                        if end > old_len { vmo_zero_range(vmo, old_len, end); }
                    }
                } else {
                    for p in new_pages..old_pages {
                        mm::pageref::unref_or_free(vmo.pages[p], 0);
                    }
                    vmo.pages.truncate(new_pages);
                }
                vmo.len = new_len;
                tmp[idx].len = new_len; // mirror EOF
                return ok_reply();
            }
            let entry = &mut tmp[idx];
            if new_len > MAX_TMP_SIZE { return err_reply(-28); }
            if new_len > entry.len { for b in &mut entry.data[entry.len..new_len] { *b = 0; } }
            entry.len = new_len;
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

/// fsync(fd) — flush the filesystem backing `fd`.
///
/// Only a mounted filesystem has anything to flush. tmpfs, procfs, pipes and
/// device nodes hold no write-back state that could survive a reset, so
/// reporting success for them is accurate, not a shortcut.
fn handle_fsync(pid: u32, fd: usize) -> Message {
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); } // EBADF
    match tbl.fds[fd].kind {
        VnodeKind::MountedFile { port, .. } => {
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_FSYNC;
            call_port(port, proxy)
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

fn handle_rename(old_ptr: usize, new_ptr: usize) -> Message {
    let (obuf, olen) = match read_cstr_raw(old_ptr) { Some(r) => r, None => return err_reply(-14) };
    let (nbuf, nlen) = match read_cstr_raw(new_ptr) { Some(r) => r, None => return err_reply(-14) };

    match (tmpfs_path(&obuf[..olen]), tmpfs_path(&nbuf[..nlen])) {
        (Some(old), Some(new)) => tmpfs_rename(old, new),
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
fn tmpfs_rename(old: &[u8], new: &[u8]) -> Message {
    if old == new { return ok_reply(); }
    if is_tmpfs_root(old) || is_tmpfs_root(new) { return err_reply(-16); } // EBUSY — mount root
    if new.len() > MAX_TMP_PATH - 1 { return err_reply(-36); }     // ENAMETOOLONG

    let open_fds = tmp_open_fd_mask(); // before TMP_FILES: FD_TABLES → TMP_FILES
    let mut tmp = TMP_FILES.lock();
    let idx = match tmp_find(&tmp[..], old) { Some(i) => i, None => return err_reply(-2) };
    let src_is_dir = tmp[idx].is_dir;

    // "mv d d/sub" would detach the subtree from the namespace.
    if new.len() > old.len() && new.starts_with(old) && new[old.len()] == b'/' {
        return err_reply(-22); // EINVAL
    }
    let parent = match tmp_parent(new) { Some(p) => p, None => return err_reply(-16) };
    if !tmp_dir_exists(&tmp[..], parent) { return err_reply(-2); } // ENOENT

    // Destination handling, POSIX order: type mismatches first, then the
    // implicit removal of an existing target.
    if let Some(didx) = tmp_find(&tmp[..], new) {
        let dst_is_dir = tmp[didx].is_dir;
        if src_is_dir && !dst_is_dir { return err_reply(-20); }  // ENOTDIR
        if !src_is_dir && dst_is_dir { return err_reply(-21); }  // EISDIR
        if dst_is_dir && tmp_has_descendants(&tmp[..], new, didx) {
            return err_reply(-39); // ENOTEMPTY
        }
        // Clobbering the destination drops one name, not necessarily the file:
        // if the victim was hard-linked elsewhere its bytes must survive.
        tmp_drop_name(&mut tmp[..], didx, open_fds);
    }

    if !src_is_dir {
        tmp_set_path(&mut tmp[idx], new);
        return ok_reply();
    }

    // Directory: check every descendant fits under the new prefix *before*
    // mutating anything, so a failure leaves the pool untouched.
    let grow = new.len() as isize - old.len() as isize;
    for (i, e) in tmp.iter().enumerate() {
        if i == idx || !e.in_use || e.ephemeral { continue; }
        if e.path_len > old.len() && &e.path[..old.len()] == old && e.path[old.len()] == b'/' {
            if (e.path_len as isize + grow) as usize > MAX_TMP_PATH - 1 {
                return err_reply(-36); // ENAMETOOLONG
            }
        }
    }
    let mut buf = [0u8; MAX_TMP_PATH];
    for i in 0..MAX_TMP_FILES {
        if i == idx || !tmp[i].in_use || tmp[i].ephemeral { continue; }
        let plen = tmp[i].path_len;
        if plen <= old.len() || &tmp[i].path[..old.len()] != old || tmp[i].path[old.len()] != b'/' {
            continue;
        }
        let tail_len = plen - old.len();
        buf[..new.len()].copy_from_slice(new);
        buf[new.len()..new.len() + tail_len].copy_from_slice(&tmp[i].path[old.len()..plen]);
        let total = new.len() + tail_len;
        tmp_set_path(&mut tmp[i], &buf[..total]);
    }
    tmp_set_path(&mut tmp[idx], new);
    ok_reply()
}

fn handle_unlink(path_ptr: usize) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];

    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-21); } // EISDIR — mount root
        let open_fds = tmp_open_fd_mask(); // before TMP_FILES: FD_TABLES → TMP_FILES
        let mut tmp = TMP_FILES.lock();
        return match tmp_find(&tmp[..], path) {
            Some(idx) if tmp[idx].is_dir => err_reply(-21), // EISDIR — use rmdir()
            // Drops the *name*. The bytes go only when the last name does —
            // see tmp_drop_name. A symlink lands here too (the choke point
            // deliberately did not follow the final component), so `rm l`
            // removes the link and never the file it points at.
            Some(idx) => { tmp_drop_name(&mut tmp[..], idx, open_fds); ok_reply() }
            None      => err_reply(-2),
        };
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

    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-17); }              // EEXIST — mount root
        if path.len() > MAX_TMP_PATH - 1 { return err_reply(-36); }    // ENAMETOOLONG
        let mut tmp = TMP_FILES.lock();
        if tmp_find(&tmp[..], path).is_some() { return err_reply(-17); }
        // Intermediate components must already exist — `mkdir -p` creates
        // them outermost-first, so this is the check that makes it correct
        // rather than silently producing an orphaned "/tmp/a/b".
        match tmp_parent(path) {
            Some(p) if tmp_dir_exists(&tmp[..], p) => {}
            _ => return err_reply(-2), // ENOENT
        }
        let idx = match tmp.iter().position(|e| !e.in_use) {
            Some(i) => i,
            None    => return err_reply(-28), // ENOSPC
        };
        tmp[idx] = TmpFileEntry::empty();
        tmp[idx].in_use = true;
        tmp[idx].is_dir = true;
        tmp[idx].mode = mode & 0o777 & !sched::umask(u32::MAX);
        tmp[idx].uid  = sched::euid_of(pid);
        tmp[idx].gid  = sched::egid_of(pid);
        tmp_set_path(&mut tmp[idx], path);
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
fn handle_mknod(pid: u32, path_ptr: usize, mode: u32) -> Message {
    const S_IFMT:  u32 = 0o170000;
    const S_IFIFO: u32 = 0o010000;

    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];

    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-17); }              // EEXIST — mount root
        if path.len() > MAX_TMP_PATH - 1 { return err_reply(-36); }    // ENAMETOOLONG
        let mut tmp = TMP_FILES.lock();
        if tmp_find(&tmp[..], path).is_some() { return err_reply(-17); } // EEXIST
        match tmp_parent(path) {
            Some(p) if tmp_dir_exists(&tmp[..], p) => {}
            _ => return err_reply(-2), // ENOENT
        }
        let idx = match tmp.iter().position(|e| !e.in_use) {
            Some(i) => i,
            None    => return err_reply(-28), // ENOSPC
        };
        tmp[idx] = TmpFileEntry::empty();
        tmp[idx].in_use  = true;
        tmp[idx].is_dir  = false;
        tmp[idx].is_fifo = mode & S_IFMT == S_IFIFO;
        tmp[idx].mode = mode & 0o777 & !sched::umask(u32::MAX);
        tmp[idx].uid  = sched::euid_of(pid);
        tmp[idx].gid  = sched::egid_of(pid);
        tmp_set_path(&mut tmp[idx], path);
        return ok_reply();
    }
    for &dir in RAMFS_DIRS { if raw == dir { return err_reply(-17); } }
    err_reply(-30) // EROFS
}

// ── AF_UNIX socket nodes (called from the net server) ────────────────────────
//
// A pathname AF_UNIX bind creates a real S_IFSOCK node on tmpfs; connect
// resolves the path through the same lookup machinery (symlinks, the
// /tmp/-/dev/shm/-/run/user/0 mounts) back to the net server's listener.
// Abstract-namespace sockets never reach here (net matches those by bytes).

/// bind(): create an S_IFSOCK node at `path`, tagged with the net server's
/// `sock_id`. Intermediate components are followed through symlinks; the final
/// component is created literally (bind does not follow a trailing symlink).
///
/// 0 on success, else a negative errno: -17 EEXIST (net → EADDRINUSE), -2
/// ENOENT (missing parent), -36 ENAMETOOLONG, -28 ENOSPC, -95 EOPNOTSUPP (not
/// a tmpfs path — f2fs socket binds are unsupported).
pub fn unix_bind_node(pid: u32, path: &[u8], sock_id: u64) -> i32 {
    let mut resolved = [0u8; 256];
    let rpath = match tmp_resolve_links(path, false, &mut resolved) {
        Ok(n)  => &resolved[..n],
        Err(e) => return e,
    };
    let tpath = match tmpfs_path(rpath) {
        Some(p) => p,
        None    => return -95, // EOPNOTSUPP — sockets only bind on tmpfs
    };
    if is_tmpfs_root(tpath) { return -17; }             // EEXIST — the mount root
    if tpath.len() > MAX_TMP_PATH - 1 { return -36; }   // ENAMETOOLONG
    let mut tmp = TMP_FILES.lock();
    if tmp_find(&tmp[..], tpath).is_some() { return -17; } // EEXIST
    match tmp_parent(tpath) {
        Some(p) if tmp_dir_exists(&tmp[..], p) => {}
        _ => return -2, // ENOENT
    }
    let idx = match tmp.iter().position(|e| !e.in_use) {
        Some(i) => i, None => return -28, // ENOSPC
    };
    tmp[idx] = TmpFileEntry::empty();
    tmp[idx].in_use  = true;
    tmp[idx].is_sock = true;
    tmp[idx].sock_id = sock_id;
    tmp[idx].mode = 0o777 & !sched::umask(u32::MAX);
    tmp[idx].uid  = sched::euid_of(pid);
    tmp[idx].gid  = sched::egid_of(pid);
    tmp_set_path(&mut tmp[idx], tpath);
    0
}

/// connect(): resolve `path` (following symlinks on every component) to the
/// `sock_id` of the S_IFSOCK node bound there. Returns the sock_id (>= 0) or a
/// negative errno: -2 ENOENT (nothing there), -111 ECONNREFUSED (exists but is
/// not a socket), -95 EOPNOTSUPP (not a tmpfs path).
pub fn unix_resolve_node(_pid: u32, path: &[u8]) -> i64 {
    let mut resolved = [0u8; 256];
    let rpath = match tmp_resolve_links(path, true, &mut resolved) {
        Ok(n)  => &resolved[..n],
        Err(e) => return e as i64,
    };
    let tpath = match tmpfs_path(rpath) {
        Some(p) => p,
        None    => return -95,
    };
    let tmp = TMP_FILES.lock();
    match tmp_find(&tmp[..], tpath) {
        Some(idx) if tmp[idx].is_sock => tmp[idx].sock_id as i64,
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
    let (tbuf, tlen) = match read_cstr_raw(target_ptr) { Some(r) => r, None => return err_reply(-14) };
    let (lbuf, llen) = match read_cstr_raw(link_ptr)   { Some(r) => r, None => return err_reply(-14) };
    if tlen == 0 { return err_reply(-2); } // ENOENT — empty target
    let raw = &lbuf[..llen];

    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-17); }           // EEXIST — mount root
        if path.len() > MAX_TMP_PATH - 1 { return err_reply(-36); } // ENAMETOOLONG
        let mut tmp = TMP_FILES.lock();
        if tmp_find(&tmp[..], path).is_some() { return err_reply(-17); } // EEXIST
        match tmp_parent(path) {
            Some(p) if tmp_dir_exists(&tmp[..], p) => {}
            _ => return err_reply(-2), // ENOENT
        }
        let idx = match tmp.iter().position(|e| !e.in_use) {
            Some(i) => i,
            None    => return err_reply(-28), // ENOSPC
        };
        tmp[idx] = TmpFileEntry::empty();
        tmp[idx].in_use  = true;
        tmp[idx].is_link = true;
        // Symlink permission bits are 0777 everywhere and are never consulted;
        // the target's bits are what govern access.
        tmp[idx].mode = 0o777;
        tmp[idx].uid  = sched::euid_of(pid);
        tmp[idx].gid  = sched::egid_of(pid);
        tmp[idx].len  = tlen;
        tmp[idx].data[..tlen].copy_from_slice(&tbuf[..tlen]);
        tmp_set_path(&mut tmp[idx], path);
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

    if let Some(path) = tmpfs_path(raw) {
        let tmp = TMP_FILES.lock();
        return match tmp_find(&tmp[..], path) {
            Some(idx) if tmp[idx].is_link => {
                let n = tmp[idx].len.min(buf_len);
                unsafe { core::ptr::copy_nonoverlapping(tmp[idx].data.as_ptr(), buf_ptr as *mut u8, n); }
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
fn handle_link(old_ptr: usize, new_ptr: usize) -> Message {
    let (obuf, olen) = match read_cstr_raw(old_ptr) { Some(r) => r, None => return err_reply(-14) };
    let (nbuf, nlen) = match read_cstr_raw(new_ptr) { Some(r) => r, None => return err_reply(-14) };
    let (oraw, nraw) = (&obuf[..olen], &nbuf[..nlen]);

    let (otmp, ntmp) = (tmpfs_path(oraw), tmpfs_path(nraw));
    match (otmp, ntmp) {
        (Some(old), Some(new)) => {
            if new.len() > MAX_TMP_PATH - 1 { return err_reply(-36); } // ENAMETOOLONG
            let mut tmp = TMP_FILES.lock();
            let src = match tmp_find(&tmp[..], old) { Some(i) => i, None => return err_reply(-2) };
            if tmp[src].is_dir { return err_reply(-1); } // EPERM
            if tmp_find(&tmp[..], new).is_some() { return err_reply(-17); } // EEXIST
            match tmp_parent(new) {
                Some(p) if tmp_dir_exists(&tmp[..], p) => {}
                _ => return err_reply(-2), // ENOENT
            }
            let owner = tmp_owner(&tmp[..], src);
            let idx = match tmp.iter().position(|e| !e.in_use) {
                Some(i) => i,
                None    => return err_reply(-28), // ENOSPC
            };
            tmp[idx] = TmpFileEntry::empty();
            tmp[idx].in_use  = true;
            // The alias carries no bytes of its own — everything that reads
            // content goes through tmp_owner() to `owner`. Mode/uid/gid are
            // mirrored only so a lock-free peek at the slot isn't nonsense;
            // stat() reads them from the owner regardless.
            tmp[idx].link_to = owner;
            tmp[idx].is_fifo = tmp[owner].is_fifo;
            tmp[idx].is_link = tmp[owner].is_link;
            tmp[idx].is_sock = tmp[owner].is_sock;
            tmp[idx].sock_id = tmp[owner].sock_id;
            tmp[idx].mode    = tmp[owner].mode;
            tmp[idx].uid     = tmp[owner].uid;
            tmp[idx].gid     = tmp[owner].gid;
            tmp_set_path(&mut tmp[idx], new);
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

fn handle_rmdir(path_ptr: usize) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];

    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-16); } // EBUSY — mount root
        let mut tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp[..], path) { Some(i) => i, None => return err_reply(-2) };
        if !tmp[idx].is_dir { return err_reply(-20); } // ENOTDIR
        if tmp_has_descendants(&tmp[..], path, idx) { return err_reply(-39); } // ENOTEMPTY
        tmp[idx] = TmpFileEntry::empty();
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
        let euid = sched::euid_of(pid);
        let mut tmp = TMP_FILES.lock();
        return match tmp_find(&tmp[..], path) {
            Some(idx) => {
                let owner = tmp_owner(&tmp[..], idx);
                if euid != 0 && euid != tmp[owner].uid { return err_reply(-1); } // EPERM
                tmp[owner].mode = mode & 0o777;
                tmp_acl_chmod_sync(&mut tmp[owner], mode);
                ok_reply()
            }
            None => err_reply(-2), // ENOENT
        };
    }
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

fn handle_fchmod(pid: u32, fd: usize, mode: u32) -> Message {
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => {
            drop(tbls);
            let euid = sched::euid_of(pid);
            let mut tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp[..], idx);
            if euid != 0 && euid != tmp[owner].uid { return err_reply(-1); } // EPERM
            tmp[owner].mode = mode & 0o777;
            tmp_acl_chmod_sync(&mut tmp[owner], mode);
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
        _ => err_reply(-1), // EPERM — devices/RAMFS have fixed, root-owned modes
    }
}

/// `u32::MAX` for `uid`/`gid` means "leave unchanged" (mirrors chown(2)'s `-1`).
/// See `handle_chmod` for what `follow` does — `lchown(2)` is the common
/// caller of the false case.
fn handle_chown(pid: u32, path_ptr: usize, uid: u32, gid: u32, follow: bool) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if let Some(path) = tmpfs_path(raw) {
        let euid = sched::euid_of(pid);
        let mut tmp = TMP_FILES.lock();
        return match tmp_find(&tmp[..], path) {
            Some(idx) => apply_chown(&mut tmp[idx], euid, uid, gid),
            None => err_reply(-2), // ENOENT
        };
    }
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
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => {
            drop(tbls);
            let euid = sched::euid_of(pid);
            let mut tmp = TMP_FILES.lock();
            apply_chown(&mut tmp[idx], euid, uid, gid)
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
        _ => err_reply(-1),
    }
}

/// Only root may change the owning uid; the owner (or root) may change gid.
fn apply_chown(e: &mut TmpFileEntry, euid: u32, uid: u32, gid: u32) -> Message {
    if uid != u32::MAX {
        if euid != 0 { return err_reply(-1); } // EPERM
        e.uid = uid;
    }
    if gid != u32::MAX {
        if euid != 0 && euid != e.uid { return err_reply(-1); } // EPERM
        e.gid = gid;
    }
    ok_reply()
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

/// posix_acl_chmod: after a mode change, rewrite any stored *access* ACL so its
/// USER_OBJ / mask-or-GROUP_OBJ / OTHER entries track the new owner/group/other
/// bits. Absent (or trivial, hence unstored) ACLs make this a no-op.
fn tmp_acl_chmod_sync(e: &mut TmpFileEntry, new_mode: u32) {
    let mut buf = [0u8; 256];
    let len = match xattr::find(&e.xattr, xattr::IDX_ACL_ACCESS, b"") {
        Some(v) if v.len() <= buf.len() => { buf[..v.len()].copy_from_slice(v); v.len() }
        _ => return,
    };
    xattr::acl_chmod_rewrite(&mut buf[..len], new_mode as u16);
    let _ = xattr::set(&mut e.xattr, xattr::IDX_ACL_ACCESS, b"", &buf[..len], 0);
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

fn tmp_setxattr_local(e: &mut TmpFileEntry, euid: u32, egid: u32,
                      name_ptr: usize, val_ptr: usize, size: usize, flags: u32) -> Message {
    let (nbuf, nlen) = match read_cstr_raw(name_ptr) { Some(r) => r, None => return err_reply(-14) };
    if nlen == 0 || nlen > xattr::XATTR_NAME_MAX { return err_reply(-xattr::ERANGE); }
    let (idx, suf) = match xattr::split_name(&nbuf[..nlen]) {
        Some(v) => v, None => return err_reply(-xattr::EOPNOTSUPP),
    };
    let meta = tmp_meta(e);
    let acl = xattr::find(&e.xattr, xattr::IDX_ACL_ACCESS, b"");
    if let Err(er) = xattr::may_write_xattr(idx, &meta, euid, egid, acl) {
        return err_reply(-er);
    }
    // The ACL namespaces keep the inode mode in lock-step with their permission
    // bits (posix_acl semantics); user.*/trusted.* are opaque blobs.
    if idx == xattr::IDX_ACL_ACCESS || idx == xattr::IDX_ACL_DEFAULT {
        // An empty value clears the ACL (setfacl -b/-k). Absence is not an
        // error — the requested end state ("no such ACL") already holds.
        if size == 0 {
            let _ = xattr::remove(&mut e.xattr, idx, suf);
            return ok_reply();
        }
        let val = unsafe { core::slice::from_raw_parts(val_ptr as *const u8, size) };
        let summary = match xattr::acl_validate(val) {
            Ok(s) => s, Err(_) => return err_reply(-xattr::EINVAL),
        };
        if idx == xattr::IDX_ACL_ACCESS {
            let bits = xattr::acl_mode_bits(&summary) as u32 & 0o777;
            if xattr::acl_is_trivial(&summary) {
                // Fully representable as mode bits: fold into the mode and store
                // nothing (Linux drops a trivial access ACL).
                e.mode = (e.mode & !0o777) | bits;
                let _ = xattr::remove(&mut e.xattr, xattr::IDX_ACL_ACCESS, b"");
                return ok_reply();
            }
            if let Err(er) = xattr::set(&mut e.xattr, idx, suf, val, flags) {
                return err_reply(-er);
            }
            e.mode = (e.mode & !0o777) | bits;
            return ok_reply();
        }
        // IDX_ACL_DEFAULT: the dir-only guard already ran in may_write_xattr.
        return match xattr::set(&mut e.xattr, idx, suf, val, flags) {
            Ok(_) => ok_reply(), Err(er) => err_reply(-er),
        };
    }
    let val: &[u8] = if size == 0 { &[] }
                     else { unsafe { core::slice::from_raw_parts(val_ptr as *const u8, size) } };
    match xattr::set(&mut e.xattr, idx, suf, val, flags) {
        Ok(_) => ok_reply(), Err(er) => err_reply(-er),
    }
}

fn tmp_getxattr_local(e: &TmpFileEntry, euid: u32, egid: u32,
                      name_ptr: usize, val_ptr: usize, size: usize) -> Message {
    let (nbuf, nlen) = match read_cstr_raw(name_ptr) { Some(r) => r, None => return err_reply(-14) };
    if nlen == 0 || nlen > xattr::XATTR_NAME_MAX { return err_reply(-xattr::ERANGE); }
    let (idx, suf) = match xattr::split_name(&nbuf[..nlen]) {
        Some(v) => v, None => return err_reply(-xattr::EOPNOTSUPP),
    };
    let meta = tmp_meta(e);
    let acl = xattr::find(&e.xattr, xattr::IDX_ACL_ACCESS, b"");
    if let Err(er) = xattr::may_read_xattr(idx, &meta, euid, egid, acl) {
        return err_reply(-er);
    }
    let value = match xattr::find(&e.xattr, idx, suf) {
        Some(v) => v, None => return err_reply(-xattr::ENODATA),
    };
    let len = value.len();
    if size == 0 { return val_reply(len as u64); }
    if size < len { return err_reply(-xattr::ERANGE); }
    unsafe { core::ptr::copy_nonoverlapping(value.as_ptr(), val_ptr as *mut u8, len); }
    val_reply(len as u64)
}

fn tmp_listxattr_local(e: &TmpFileEntry, euid: u32, list_ptr: usize, size: usize) -> Message {
    // O(1) fast path — the `ls -l` stat storm hits this on every entry.
    if xattr::is_empty(&e.xattr) { return val_reply(0); }
    let out: &mut [u8] = if size == 0 { &mut [] }
                         else { unsafe { core::slice::from_raw_parts_mut(list_ptr as *mut u8, size) } };
    match xattr::list(&e.xattr, out, euid == 0) {
        Ok(n) => val_reply(n as u64), Err(er) => err_reply(-er),
    }
}

fn tmp_removexattr_local(e: &mut TmpFileEntry, euid: u32, egid: u32, name_ptr: usize) -> Message {
    let (nbuf, nlen) = match read_cstr_raw(name_ptr) { Some(r) => r, None => return err_reply(-14) };
    if nlen == 0 || nlen > xattr::XATTR_NAME_MAX { return err_reply(-xattr::ERANGE); }
    let (idx, suf) = match xattr::split_name(&nbuf[..nlen]) {
        Some(v) => v, None => return err_reply(-xattr::EOPNOTSUPP),
    };
    let meta = tmp_meta(e);
    let acl = xattr::find(&e.xattr, xattr::IDX_ACL_ACCESS, b"");
    if let Err(er) = xattr::may_write_xattr(idx, &meta, euid, egid, acl) {
        return err_reply(-er);
    }
    // ACL removal needs no mode resync — the mode stays as it is (Linux).
    match xattr::remove(&mut e.xattr, idx, suf) {
        Ok(_) => ok_reply(), Err(er) => err_reply(-er),
    }
}

// ── path forms (arg0 is a path; l-forms already resolved by path_args) ────────

fn handle_setxattr(pid: u32, tag: u64, path_ptr: usize, name_ptr: usize,
                   val_ptr: usize, size: usize, flags: u32) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if let Some(path) = tmpfs_path(raw) {
        let (euid, egid) = (sched::euid_of(pid), sched::egid_of(pid));
        let mut tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp[..], path) { Some(i) => i, None => return err_reply(-2) };
        let owner = tmp_owner(&tmp[..], idx);
        return tmp_setxattr_local(&mut tmp[owner], euid, egid, name_ptr, val_ptr, size, flags);
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
        let (euid, egid) = (sched::euid_of(pid), sched::egid_of(pid));
        let tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp[..], path) { Some(i) => i, None => return err_reply(-2) };
        let owner = tmp_owner(&tmp[..], idx);
        return tmp_getxattr_local(&tmp[owner], euid, egid, name_ptr, val_ptr, size);
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
        let euid = sched::euid_of(pid);
        let tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp[..], path) { Some(i) => i, None => return err_reply(-2) };
        let owner = tmp_owner(&tmp[..], idx);
        return tmp_listxattr_local(&tmp[owner], euid, list_ptr, size);
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
        let (euid, egid) = (sched::euid_of(pid), sched::egid_of(pid));
        let mut tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp[..], path) { Some(i) => i, None => return err_reply(-2) };
        let owner = tmp_owner(&tmp[..], idx);
        return tmp_removexattr_local(&mut tmp[owner], euid, egid, name_ptr);
    }
    if let Some(port) = find_mount_port(raw) {
        return xattr_proxy(port, tag, path_ptr as u64, name_ptr as u64, 0, 0, 0);
    }
    err_reply(-95)
}

/// faccessat(2): permission probe honoring any stored access ACL. `amode==0`
/// (F_OK) is pure existence. A path this server does not own answers -38 so the
/// kernel's legacy fallback runs.
fn handle_access(pid: u32, path_ptr: usize, amode: u32) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if let Some(path) = tmpfs_path(raw) {
        let (euid, egid) = (sched::euid_of(pid), sched::egid_of(pid));
        let tmp = TMP_FILES.lock();
        // Not a pool entry (e.g. "/tmp" itself, a ramfs dir): -38 so the
        // kernel's stat-based fallback answers, exactly as before VFS_ACCESS
        // existed. A genuinely missing file still ends up ENOENT there.
        let idx = match tmp_find(&tmp[..], path) { Some(i) => i, None => return err_reply(-38) };
        let owner = tmp_owner(&tmp[..], idx);
        if amode == 0 { return ok_reply(); } // F_OK: existence only
        let e = &tmp[owner];
        let meta = tmp_meta(e);
        let acl = xattr::find(&e.xattr, xattr::IDX_ACL_ACCESS, b"");
        let ok = xattr::access_check(&meta, euid, egid, acl,
                                     amode & 4 != 0, amode & 2 != 0, amode & 1 != 0);
        return if ok { ok_reply() } else { err_reply(-13) }; // EACCES
    }
    if let Some(port) = find_mount_port(raw) {
        return xattr_proxy(port, VFS_ACCESS, path_ptr as u64, amode as u64, 0, 0, 0);
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
        let mut tbls = FD_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
        tbl.fds[fd].kind
    };
    match kind {
        VnodeKind::TmpFile { idx, .. } => {
            let (euid, egid) = (sched::euid_of(pid), sched::egid_of(pid));
            let mut tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp[..], idx);
            tmp_setxattr_local(&mut tmp[owner], euid, egid, name_ptr, val_ptr, size, flags)
        }
        VnodeKind::MountedFile { port, file_id } =>
            xattr_proxy(port, VFS_FSETXATTR, file_id as u64, name_ptr as u64,
                        val_ptr as u64, size as u64, flags as u64),
        _ => err_reply(-95),
    }
}

fn handle_fgetxattr(pid: u32, fd: usize, name_ptr: usize, val_ptr: usize, size: usize) -> Message {
    let kind = {
        let mut tbls = FD_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
        tbl.fds[fd].kind
    };
    match kind {
        VnodeKind::TmpFile { idx, .. } => {
            let (euid, egid) = (sched::euid_of(pid), sched::egid_of(pid));
            let tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp[..], idx);
            tmp_getxattr_local(&tmp[owner], euid, egid, name_ptr, val_ptr, size)
        }
        VnodeKind::MountedFile { port, file_id } =>
            xattr_proxy(port, VFS_FGETXATTR, file_id as u64, name_ptr as u64, val_ptr as u64, size as u64, 0),
        _ => err_reply(-95),
    }
}

fn handle_flistxattr(pid: u32, fd: usize, list_ptr: usize, size: usize) -> Message {
    let kind = {
        let mut tbls = FD_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
        tbl.fds[fd].kind
    };
    match kind {
        VnodeKind::TmpFile { idx, .. } => {
            let euid = sched::euid_of(pid);
            let tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp[..], idx);
            tmp_listxattr_local(&tmp[owner], euid, list_ptr, size)
        }
        VnodeKind::MountedFile { port, file_id } =>
            xattr_proxy(port, VFS_FLISTXATTR, file_id as u64, list_ptr as u64, size as u64, 0, 0),
        _ => err_reply(-95),
    }
}

fn handle_fremovexattr(pid: u32, fd: usize, name_ptr: usize) -> Message {
    let kind = {
        let mut tbls = FD_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
        tbl.fds[fd].kind
    };
    match kind {
        VnodeKind::TmpFile { idx, .. } => {
            let (euid, egid) = (sched::euid_of(pid), sched::egid_of(pid));
            let mut tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp[..], idx);
            tmp_removexattr_local(&mut tmp[owner], euid, egid, name_ptr)
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
    unsafe {
        let p = stat_ptr as *mut u8;
        core::ptr::write_bytes(p, 0, STAT_SIZE);
        (p.add(0) as *mut u64).write_unaligned(1u64); // st_dev
        (p.add(8) as *mut u64).write_unaligned(ino);  // st_ino

        #[cfg(target_arch = "x86_64")]
        {
            (p.add(16) as *mut u64).write_unaligned(nlink); // st_nlink (u64)
            (p.add(24) as *mut u32).write_unaligned(mode);  // st_mode
            (p.add(28) as *mut u32).write_unaligned(uid);   // st_uid
            (p.add(32) as *mut u32).write_unaligned(gid);   // st_gid
            (p.add(56) as *mut i64).write_unaligned(4096i64); // st_blksize (long)
        }
        #[cfg(target_arch = "aarch64")]
        {
            (p.add(16) as *mut u32).write_unaligned(mode);         // st_mode
            (p.add(20) as *mut u32).write_unaligned(nlink as u32); // st_nlink (u32)
            (p.add(24) as *mut u32).write_unaligned(uid);          // st_uid
            (p.add(28) as *mut u32).write_unaligned(gid);          // st_gid
            (p.add(56) as *mut i32).write_unaligned(4096i32);      // st_blksize (int)
        }

        (p.add(48) as *mut u64).write_unaligned(size);                // st_size
        (p.add(64) as *mut u64).write_unaligned((size + 511) / 512);  // st_blocks
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
        let p = buf_ptr as *mut u8;
        core::ptr::write_bytes(p, 0, STATFS_SIZE);
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
    // pivot_root has made "/" a prefix match for everything.
    if path.starts_with(b"/proc") || path.starts_with(b"/dev") || path.starts_with(b"/sys") {
        write_statfs(buf_ptr, &procfs_statfs());
        return statfs_reply();
    }
    if !is_tmp_path(path) {
        if let Some(port) = find_mount_port(path) {
            let mut proxy = Message::empty();
            proxy.tag = VFS_STATFS;
            proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            return call_port(port, proxy);
        }
    }
    write_statfs(buf_ptr, &tmpfs_statfs());
    statfs_reply()
}

/// fstatfs(fd) — same answer as `handle_statfs`, selected by descriptor.
///
/// Only a `MountedFile` can name a real volume; every other vnode kind lives
/// in this server's own memory, so it reports the tmpfs pool.
fn handle_fstatfs(pid: u32, fd: usize, buf_ptr: usize) -> Message {
    if buf_ptr == 0 { return err_reply(-14); }
    let port = {
        let mut tbls = FD_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
        match tbl.fds[fd].kind {
            VnodeKind::MountedFile { port, .. } => Some(port),
            _ => None,
        }
    };
    if let Some(port) = port {
        let mut proxy = Message::empty();
        proxy.tag = VFS_STATFS;
        proxy.data[0..8].copy_from_slice(&0u64.to_le_bytes()); // no path — port is the volume
        proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
        return call_port(port, proxy);
    }
    write_statfs(buf_ptr, &tmpfs_statfs());
    statfs_reply()
}

/// Live figures for the tmpfs pool: `MAX_TMP_FILES` slots of `MAX_TMP_SIZE`
/// bytes each, counted in 4 KiB blocks. These are real, not invented — the
/// pool is a fixed BSS array, so its capacity *is* the filesystem size and the
/// used byte count is exact.
fn tmpfs_statfs() -> StatfsVals {
    const BSIZE: u64 = 4096;
    let total_blocks = (MAX_TMP_FILES * MAX_TMP_SIZE) as u64 / BSIZE;
    let (used_bytes, used_slots) = {
        let tmp = TMP_FILES.lock();
        let mut bytes = 0u64;
        let mut slots = 0u64;
        for e in tmp.iter() {
            if !e.in_use { continue; }
            slots += 1;
            // Aliases (link_to != MAX) carry no bytes of their own — counting
            // them would charge a hard-linked file to the volume twice.
            if !e.is_dir && e.link_to == usize::MAX { bytes += e.len as u64; }
        }
        (bytes, slots)
    };
    let used_blocks = (used_bytes + BSIZE - 1) / BSIZE;
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
        namelen: (MAX_TMP_PATH - 1) as u64,
    }
}

/// /proc and /dev: zero-capacity pseudo-filesystems, exactly as Linux reports
/// them. `df` filters these out by fstype long before f_blocks matters, and
/// `df /proc` prints a 0-block line rather than an error — both match Linux.
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
        let mut tbls = FD_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
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
        return call_port(port, proxy);
    }

    // (mode, size, ino)
    let (mode, size, ino): (u32, u64, u64) = match kind {
        VnodeKind::Pipe { ring, .. } => {
            // st_size on a FIFO is 0 on Linux; the ring index is a stable,
            // unique-per-pipe inode number, which is what `pipe:[N]` in
            // /proc/self/fd already reports.
            (S_IFIFO | 0o600, 0, 0x1000_0000 + ring as u64)
        }
        // A console proxy (a dup'd stdio fd, or an fd opened on /dev/tty or
        // /dev/stdin) is the console, so it reports the console's inode — the
        // same one stat("/dev/console") reports. Without that agreement
        // ttyname() rejects the fd; see CONSOLE_INO.
        VnodeKind::DevStdio { .. } => (S_IFCHR | 0o666, 0, CONSOLE_INO),
        VnodeKind::DevNull | VnodeKind::DevZero | VnodeKind::DevUrandom
        | VnodeKind::DevFb { .. }
        | VnodeKind::DynamicDevice { .. } => (S_IFCHR | 0o666, 0, 0),
        // A pseudo-directory reports S_IFDIR with size 0. It used to report
        // S_IFREG with `size = data.len()`, i.e. the length of its own path —
        // which is what let memmap2 map `/tmp` as a 4-byte "file".
        VnodeKind::RamFile { is_dir: true, .. } => (S_IFDIR | 0o755, 0, 0),
        VnodeKind::RamFile { data, .. } => (S_IFREG | 0o644, data.len() as u64, 0),
        VnodeKind::TmpFile { idx, .. } => {
            let t = TMP_FILES.lock();
            let e = &t[idx];
            if e.is_dir       { (S_IFDIR | (e.mode & 0o7777), 0, 0x2000_0000 + idx as u64) }
            else if e.is_sock { (S_IFSOCK | (e.mode & 0o7777), 0, 0x2000_0000 + idx as u64) }
            else if e.is_fifo { (S_IFIFO | (e.mode & 0o7777), 0, 0x2000_0000 + idx as u64) }
            else              { (S_IFREG | (e.mode & 0o7777), e.len as u64, 0x2000_0000 + idx as u64) }
        }
        // eventfd/timerfd are anon-inode files on Linux and report S_IFREG.
        VnodeKind::EventFd { .. } | VnodeKind::TimerFd { .. } => (S_IFREG | 0o600, 0, 0),
        // Handled by the early return above (proxied to the owning mount);
        // this arm exists only for match exhaustiveness.
        VnodeKind::MountedFile { .. } => return err_reply(-9),
        VnodeKind::None => return err_reply(-9),
    };

    // st_nlink must agree with what path-based stat reports for the same file,
    // or `ln f g && stat f` and `ln f g && stat <fd>` disagree.
    let nlink = match kind {
        VnodeKind::TmpFile { idx, .. } => { let t = TMP_FILES.lock(); tmp_nlink(&t[..], idx) }
        _ => 1,
    };
    write_stat_full(stat_ptr, mode, nlink, size, ino, 0, 0);
    ok_reply()
}

fn handle_stat(path_ptr: usize, stat_ptr: usize) -> Message {
    stat_common(path_ptr, stat_ptr, true)
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
    if stat_ptr == 0 { return err_reply(-14); }
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let path = &pbuf[..plen];

    if let Some(lookup_path) = should_lookup_ramfs(path) {
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
            if let Some(e) = tmp_find(&tmp[..], tpath).map(|i| &tmp[i]).filter(|e| e.is_dir) {
                let ino = path_ino(tpath, 1);
                let mode = 0o040000 | if e.mode != 0 { e.mode } else { 0o755 };
                let (uid, gid) = (e.uid, e.gid);
                drop(tmp);
                write_stat_owned(stat_ptr, mode, 0, ino, uid, gid);
                return ok_reply();
            }
        }
        // The console. Must be answered before the RamFS sweep below, which
        // would otherwise report the placeholder /dev/tty entry as a zero-byte
        // S_IFREG — and must carry CONSOLE_INO so it agrees with fstat on the
        // console fd itself. See CONSOLE_INO for why ttyname() depends on it.
        if lookup_path == b"/dev/tty" || lookup_path == b"/dev/console" {
            write_stat(stat_ptr, 0o020666, 0, CONSOLE_INO);
            return ok_reply();
        }
        // Special device files.
        if lookup_path == b"/dev/null" || lookup_path == b"/dev/zero" || lookup_path == b"/dev/urandom"
           || lookup_path == b"/dev/random" || lookup_path == b"/dev/fb0" {
            write_stat(stat_ptr, 0o020666, 0, 2);
            return ok_reply();
        }
        if lookup_path == b"/dev/stdin" || lookup_path == b"/dev/stdout" || lookup_path == b"/dev/stderr" {
            write_stat(stat_ptr, 0o020666, 0, 3);
            return ok_reply();
        }
        // Dynamic devices.
        let dyn_found = {
            let devices = DYNAMIC_DEVICES.lock();
            devices.iter().any(|d| d.in_use && d.path.as_bytes() == lookup_path)
        };
        if dyn_found {
            write_stat(stat_ptr, 0o020666, 0, 4);
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
            if let Some(idx) = tmp_find(&tmp[..], tpath).filter(|&i| !tmp[i].is_dir) {
                // Hard links share one inode, so both st_ino and st_nlink must
                // come from the data-owning slot — `ls -i` and `stat` are how
                // callers verify a link took, and a per-name inode number would
                // make two links look like two files.
                let owner = tmp_owner(&tmp[..], idx);
                let nlink = tmp_nlink(&tmp[..], idx);
                let e = &tmp[owner];
                let size = e.len as u64;
                // S_IFLNK / S_IFSOCK / S_IFIFO / S_IFREG
                let ifmt: u32 = if !follow && tmp[idx].is_link { 0o120000 }
                                else if e.is_sock { 0o140000 }
                                else if e.is_fifo { 0o010000 }
                                else { 0o100000 };
                let default_mode = if ifmt == 0o120000 { 0o777 } else { 0o644 };
                let mode = ifmt | if e.mode != 0 { e.mode } else { default_mode };
                let ino = 0x2000_0000 + owner as u64;
                let (uid, gid) = (e.uid, e.gid);
                drop(tmp);
                write_stat_full(stat_ptr, mode, nlink, size, ino, uid, gid);
                return ok_reply();
            }
        }
        // initrd CPIO archive.
        if let Some(data) = find_in_initrd(lookup_path) {
            write_stat(stat_ptr, 0o100444, data.len() as u64, path_ino(lookup_path, 3));
            return ok_reply();
        }
    }

    // Fall back to mounted filesystems.
    if let Some(port) = find_mount_port(path) {
        let mut proxy = Message::empty();
        proxy.tag = if follow { VFS_STAT } else { VFS_LSTAT };
        proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
        proxy.data[8..16].copy_from_slice(&(stat_ptr as u64).to_le_bytes());
        let r = call_port(port, proxy);
        // A mount server that predates VFS_LSTAT answers ENOSYS; degrade to
        // stat rather than failing the call outright.
        if !follow && reply_val(&r) == -38 {
            let mut retry = Message::empty();
            retry.tag = VFS_STAT;
            retry.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
            retry.data[8..16].copy_from_slice(&(stat_ptr as u64).to_le_bytes());
            return call_port(port, retry);
        }
        return r;
    }

    err_reply(-2) // ENOENT
}

enum FdInfo { Static(&'static [u8]), Pipe(usize), RamData(*const u8), TmpIdx(usize),
              Mounted(u32, u32), Bad }

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
        unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), buf_ptr as *mut u8, c); }
        return val_reply(c as u64);
    }

    let info = {
        let tbls = FD_TABLES.lock();
        let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) { Some(t) => t, None => return err_reply(-9) };
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
        match &tbl.fds[fd].kind {
            VnodeKind::DevNull => FdInfo::Static(b"/dev/null"),
            VnodeKind::DevZero => FdInfo::Static(b"/dev/zero"),
            VnodeKind::Pipe { ring, .. } => FdInfo::Pipe(*ring),
            // A pseudo-directory's `data` *is* its path, so report it directly;
            // it is not in RAMFS and the reverse data-pointer lookup below
            // would answer ENOENT for it.
            VnodeKind::RamFile { data, is_dir: true, .. } => FdInfo::Static(data),
            VnodeKind::RamFile { data, .. } => FdInfo::RamData(data.as_ptr()),
            VnodeKind::TmpFile { idx, .. } => FdInfo::TmpIdx(*idx),
            VnodeKind::EventFd { .. } => FdInfo::Static(b"eventfd"),
            VnodeKind::TimerFd { .. } => FdInfo::Static(b"timerfd"),
            VnodeKind::DevUrandom => FdInfo::Static(b"/dev/urandom"),
            VnodeKind::DevStdio { target_fd: 0 } => FdInfo::Static(b"/dev/stdin"),
            VnodeKind::DevStdio { target_fd: 1 } => FdInfo::Static(b"/dev/stdout"),
            VnodeKind::DevStdio { .. } => FdInfo::Static(b"/dev/stderr"),
            // A file on a mounted filesystem: only the owning mount server
            // knows the path behind its file_id, so ask it.
            VnodeKind::MountedFile { port, file_id } => FdInfo::Mounted(*port, *file_id),
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
            unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), buf_ptr as *mut u8, c); }
            val_reply(c as u64)
        }
        FdInfo::Pipe(r) => {
            let mut b = [0u8; 32]; let pref = b"pipe:["; b[..6].copy_from_slice(pref);
            let mut n = 6; let mut v = r;
            if v == 0 { b[n] = b'0'; n += 1; }
            else { let mut d = [0u8; 10]; let mut di = 0; while v > 0 { d[di] = b'0'+(v%10) as u8; di += 1; v /= 10; } for i in (0..di).rev() { b[n] = d[i]; n += 1; } }
            b[n] = b']'; n += 1;
            let c = n.min(buf_len);
            unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), buf_ptr as *mut u8, c); }
            val_reply(c as u64)
        }
        FdInfo::RamData(ptr) => {
            match RAMFS.iter().find(|e| e.data.as_ptr() == ptr) {
                Some(e) => { let c = e.path.len().min(buf_len); unsafe { core::ptr::copy_nonoverlapping(e.path.as_ptr(), buf_ptr as *mut u8, c); } val_reply(c as u64) }
                None => err_reply(-2),
            }
        }
        FdInfo::TmpIdx(i) => {
            let tmp = TMP_FILES.lock();
            if i < tmp.len() && tmp[i].in_use {
                let l = tmp[i].path_len.min(buf_len);
                unsafe { core::ptr::copy_nonoverlapping(tmp[i].path.as_ptr(), buf_ptr as *mut u8, l); }
                val_reply(l as u64)
            } else { err_reply(-9) }
        }
    }
}
