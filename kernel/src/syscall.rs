//! Syscall dispatch — the only controlled gate into kernel space.
#![allow(dead_code)]
//!
//! Syscall ABI (register mapping follows Linux on each arch):
//!   AArch64: x8 = number, x0-x5 = args, x0 = return value
//!   x86-64:  rax = number, rdi/rsi/rdx/r10/r8/r9 = args, rax = return value
//!
//! Syscall numbers match Linux ABI so that musl libc requires no patching.
//! Leandros-private syscalls (IPC, spawn) use numbers above 509.

use core::sync::atomic::{AtomicUsize, AtomicU32, AtomicU64, AtomicBool, Ordering};
use alloc::vec::Vec;
use crate::{serial_print_str, serial_write_raw, BOOT_INFO_PTR, init};
use ipc::{Message, port};
use sched::{
    fork_current, clone_thread,
    sys_sigaction, sys_sigprocmask, sys_sigaltstack, restore_signal_frame,
    current_pid, current_ppid,
    ticks, yield_now, irq_window, exit, spawn_user,
    pending_signals, replace_signal_mask,
    current_reply_port, set_current_reply_port, set_clear_child_tid,
    block_on_port_prepare, block_on_port_cancel, block_on_port_commit,
    replace_address_space,
    with_current_address_space, with_current_address_space_mut
};
#[cfg(target_arch = "x86_64")]
use sched::{set_fs_base, get_fs_base};
use sched::uaccess as ua;
use mm::paging::PageFlags;
use elf;
use vfs_server as vfs;
/// Sync-file / dma-buf fence ioctls (fd-minting, so answered here like PRIME).
#[path = "syncfile_ioctl.rs"]
mod syncfile_ioctl;
#[path = "v4l2.rs"]
pub(crate) mod v4l2;
use net_server;
use tty_server;
use evdev_server;

/// Hinted mmaps whose hint was taken and that were placed elsewhere
/// (diagnostic; logged at powers of two from 1024).
static MMAP_HINT_MOVED: AtomicUsize = AtomicUsize::new(0);

/// What a non-MAP_FIXED candidate address is, for [`mmap_place`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum MmapCand {
    /// No usable hint: the lowest free range of the address space.
    Any,
    /// The caller's hint: used if free, else placed like `Any`.
    Hint,
    /// MAP_FIXED_NOREPLACE: exactly here or not at all (the overlap check that
    /// answers EEXIST ran before the hold; a sibling thread that took the range
    /// since then makes the map itself fail).
    Exact,
}

/// Where a non-MAP_FIXED mapping of `len` bytes goes. Called inside the same
/// address-space hold as the map that follows, so no sibling thread can take
/// the range in between. None: no room.
///
/// A hint is used when that range is free; otherwise (and with no hint) the
/// mapping takes the lowest free range of this address space in the mmap
/// window (`AddressSpace::find_free_range`), so freed ranges are reused.
///
/// History: until 2026-10-19 unhinted placement was one system-wide bump
/// counter that only grew. A process cycling buffers (Firefox decoding
/// video) walked an ever-higher window, paying fresh page tables for every
/// step (~410 GiB of VA in 30 min of playback), and the counter could run
/// into a range another mapping of the same process held (lane ffpaint,
/// 2026-10-14: those mmaps used to fail ENOMEM; Linux never fails a
/// non-fixed mmap because its candidate is taken).
fn mmap_place(as_: &mm::vmm::AddressSpace, cand: usize, len: usize, kind: MmapCand) -> Option<usize> {
    match kind {
        MmapCand::Exact => Some(cand),
        MmapCand::Hint if as_.range_is_free(cand, len) => Some(cand),
        _ => {
            let at = as_.find_free_range(len)?;
            if kind == MmapCand::Hint {
                // A taken hint is routine (allocators hint just past their
                // last mapping, ~100 per boot): powers of two from 1024 on.
                let n = MMAP_HINT_MOVED.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= 1024 && n.is_power_of_two() { mmap_place_log(n, true, cand, at, len); }
            }
            Some(at)
        }
    }
}

/// One `[MMAP] range taken` line, built whole and written with one
/// `serial_print_str` (`serial_print_hex` writes its "0x" unbuffered, ahead of
/// the rest of the line, and a line written in pieces interleaves with
/// userspace output such as getty's login prompt).
fn mmap_place_log(n: usize, hint: bool, cand: usize, at: usize, len: usize) {
    struct Line { b: [u8; 160], n: usize }
    impl Line {
        fn c(&mut self, c: u8) { if self.n < self.b.len() { self.b[self.n] = c; self.n += 1; } }
        fn s(&mut self, t: &str) { for &c in t.as_bytes() { self.c(c); } }
        fn d(&mut self, mut v: usize) {
            let mut t = [0u8; 20];
            let mut i = t.len();
            loop { i -= 1; t[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
            for k in i..t.len() { self.c(t[k]); }
        }
        fn x(&mut self, v: usize) {
            self.s("0x");
            let digits = (usize::BITS - v.leading_zeros()).div_ceil(4).max(1);
            for k in (0..digits).rev() { self.c(b"0123456789abcdef"[(v >> (k * 4)) & 0xF]); }
        }
    }
    let mut l = Line { b: [0; 160], n: 0 };
    l.s("[MMAP] range taken, placed elsewhere: n="); l.d(n);
    l.s(if hint { " hint" } else { " bump" });
    l.s(" pid="); l.d(current_pid() as usize);
    l.s(" wanted="); l.x(cand);
    l.s(" at="); l.x(at);
    l.s(" len="); l.x(len);
    l.s("\n");
    crate::serial_print_str(core::str::from_utf8(&l.b[..l.n]).unwrap_or("[MMAP] range taken\n"));
}

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
    // By inode first, like a private file mapping: one registry entry per
    // executable however many processes run it, and its pages shared
    // through the page cache (`mm::pagecache`) with every other mapping of
    // the same file. The open-file slot is released. The per-open table
    // below is only the fallback when the inode registry cannot take it.
    if let Some(cap) = f2fs_server::inode_by_port(port, file_id as u64)
        .and_then(|ino| mmap_file_register(port, ino))
    {
        f2fs_server::close_by_port(port, file_id as u64);
        return Some(cap);
    }
    let mut tbl = EXEC_FILES.lock();
    for (i, slot) in tbl.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(ExecFileEntry { port, file_id, refs: 1 });
            return Some(i + 1);
        }
    }
    drop(tbl);
    // Full: ~70 live processes on a desktop of terminals outnumber the 64
    // slots, and each slot pins one of the mount's 256 open files, so it is not
    // simply raised. Back the image by inode instead, through the same
    // registry private file mappings use: deduplicated per (mount, inode),
    // pinned with `pin_inode`, and no open-file slot held. The stolen open
    // file is then released. Without this the image was loaded eagerly
    // (whole-file read into anonymous memory), per process.
    static REPORTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if !REPORTED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        crate::serial_print_str("\n[EXEC] EXEC_FILES full: further exec images are backed by inode (MMAP_FILES)\n");
    }
    let ino = f2fs_server::inode_by_port(port, file_id as u64)?;
    let cap = mmap_file_register(port, ino)?;
    f2fs_server::close_by_port(port, file_id as u64);
    Some(cap)
}

/// mm file-read hook.  Runs in page-fault context: everything below is a
/// synchronous direct call into the f2fs server (registered port handlers
/// execute in the caller's context) ending in polled virtio I/O — no
/// blocking, no rescheduling, no IPC reply ports.
fn exec_file_read(cap: usize, offset: u64, dst: *mut u8, len: usize) -> isize {
    if cap >= MMAP_CAP_BASE { return mmap_file_read(cap, offset, dst, len); }
    if cap == 0 || cap > MAX_EXEC_FILES { return -9; }
    let entry = match EXEC_FILES.lock()[cap - 1] {
        Some(e) => e,
        None    => return -9,
    };
    f2fs_server::pread_by_port(entry.port, entry.file_id as u64, dst, len, offset)
}

/// mm file-retain hook (one reference per live VMA; fork clones retain).
fn exec_file_retain(cap: usize) {
    if cap >= MMAP_CAP_BASE { return mmap_file_ref(cap, true); }
    if cap == 0 || cap > MAX_EXEC_FILES { return; }
    if let Some(ref mut e) = EXEC_FILES.lock()[cap - 1] {
        e.refs += 1;
    }
}

/// mm file-release hook; closes the mount-side file when the last VMA goes.
fn exec_file_release(cap: usize) {
    if cap >= MMAP_CAP_BASE { return mmap_file_ref(cap, false); }
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
        file_name_forget(cap);
        f2fs_server::close_by_port(e.port, e.file_id as u64);
    }
}

// ── Private file mmap(2): backing-inode registry ─────────────────────────────
//
// MAP_PRIVATE mappings of f2fs files are demand-paged exactly like exec
// images (the eager whole-file copy in sys_mmap cost ~1-1.5 s per COSMIC
// process start: each Rust binary maps its own 33-36 MiB executable to
// symbolize backtraces, ld-musl maps libgallium's 23 MiB). A mapping outlives
// its descriptor, so the registry names the *inode* (mount port + inode
// number), deduplicated — every process mapping libfoo.so shares one entry
// — and pins it in f2fs against unlink/rename-over reclaim instead of
// holding one of the mount's 256 open-file slots. Caps are MMAP_CAP_BASE +
// index, disjoint from the exec caps (1..=MAX_EXEC_FILES). `refs` counts
// live VMAs plus transient references (sys_mmap's creation reference, a
// fault's read in flight).

const MMAP_CAP_BASE: usize = 0x1_0000;
// 2048 -> grown on demand up to this ceiling (lane fdmax): a rustc process
// maps every rlib/rmeta it reads, so one big crate graph could exhaust a
// fixed 2048 inodes system-wide. Registration looks the (mount, inode) pair
// up in `MmapFiles::by_key` rather than scanning the table.
const MAX_MMAP_FILES: usize = 1 << 20;

#[derive(Clone, Copy)]
struct MmapFileEntry {
    port: u32,
    ino:  u32,
    refs: u32,
}

struct MmapFiles {
    v:      alloc::vec::Vec<Option<MmapFileEntry>>,
    /// (port, ino) -> index of its live entry.
    by_key: alloc::collections::BTreeMap<(u32, u32), usize>,
    /// Indexes of `None` entries, for reuse.
    free:   alloc::vec::Vec<usize>,
}

impl core::ops::Index<usize> for MmapFiles {
    type Output = Option<MmapFileEntry>;
    fn index(&self, i: usize) -> &Option<MmapFileEntry> { self.v.get(i).unwrap_or(&None) }
}

impl MmapFiles {
    /// Clear entry `i` (its last reference went).
    fn remove(&mut self, i: usize) {
        if let Some(Some(e)) = self.v.get(i).copied() {
            self.by_key.remove(&(e.port, e.ino));
            self.v[i] = None;
            self.free.push(i);
        }
    }
}

static MMAP_FILES: spin::Mutex<MmapFiles> = spin::Mutex::new(MmapFiles {
    v: alloc::vec::Vec::new(), by_key: alloc::collections::BTreeMap::new(), free: alloc::vec::Vec::new(),
});

/// Cap for (port, ino), taking one reference (the caller's creation ref).
fn mmap_file_register(port: u32, ino: u32) -> Option<usize> {
    let mut tbl = MMAP_FILES.lock();
    if let Some(&i) = tbl.by_key.get(&(port, ino)) {
        if let Some(e) = tbl.v[i].as_mut() { e.refs += 1; }
        return Some(MMAP_CAP_BASE + i);
    }
    let i = match tbl.free.pop() {
        Some(i) => i,
        None => {
            if tbl.v.len() >= MAX_MMAP_FILES || tbl.v.try_reserve(1).is_err() { return None; }
            tbl.v.push(None);
            tbl.v.len() - 1
        }
    };
    tbl.v[i] = Some(MmapFileEntry { port, ino, refs: 1 });
    tbl.by_key.insert((port, ino), i);
    f2fs_server::pin_inode(port, ino);
    drop(tbl);
    mm::pagecache::key_get(mmap_cache_key(port, ino));
    Some(MMAP_CAP_BASE + i)
}

/// Page-cache key of inode `ino` on the mount at `port` (never 0). The f2fs
/// server invalidates the same key when the inode's data changes.
fn mmap_cache_key(port: u32, ino: u32) -> u64 {
    f2fs_server::page_cache_key(port, ino)
}

/// mm file-key hook: inode-backed caps share pages through the page cache;
/// per-open exec caps (the fallback) do not.
fn exec_file_key(cap: usize) -> u64 {
    if cap < MMAP_CAP_BASE { return 0; }
    let i = cap - MMAP_CAP_BASE;
    if i >= MAX_MMAP_FILES { return 0; }
    match MMAP_FILES.lock()[i] {
        Some(e) => mmap_cache_key(e.port, e.ino),
        None => 0,
    }
}

fn mmap_file_ref(cap: usize, inc: bool) {
    let i = cap - MMAP_CAP_BASE;
    if i >= MAX_MMAP_FILES { return; }
    let mut tbl = MMAP_FILES.lock();
    if let Some(Some(e)) = tbl.v.get_mut(i) {
        if inc {
            e.refs += 1;
        } else {
            e.refs -= 1;
            if e.refs == 0 {
                let (port, ino) = (e.port, e.ino);
                tbl.remove(i);
                f2fs_server::unpin_inode(port, ino);
                drop(tbl);
                file_name_forget(cap);
                mm::pagecache::key_put(mmap_cache_key(port, ino));
                return;
            }
        }
    }
}

fn mmap_file_read(cap: usize, offset: u64, dst: *mut u8, len: usize) -> isize {
    let i = cap - MMAP_CAP_BASE;
    if i >= MAX_MMAP_FILES { return -9; }
    let e = match MMAP_FILES.lock()[i] {
        Some(e) => e,
        None    => return -9,
    };
    f2fs_server::pread_ino_by_port(e.port, e.ino, dst, len, offset)
}

// ── Names of mapped files (/proc/<pid>/maps) ─────────────────────────────────
//
// A cap names an inode, not a path, so the path a file was first mapped or
// executed by is recorded with its registry entry (Linux shows the dentry
// the mapping was made through, which is the same thing until a rename) and
// dropped with it.

static FILE_NAMES: spin::Mutex<alloc::collections::BTreeMap<usize, alloc::vec::Vec<u8>>> =
    spin::Mutex::new(alloc::collections::BTreeMap::new());

fn file_name_known(cap: usize) -> bool { FILE_NAMES.lock().contains_key(&cap) }

/// Record `name` for `cap` unless it has one.
fn file_name_note(cap: usize, name: &[u8]) {
    if name.is_empty() { return; }
    let mut t = FILE_NAMES.lock();
    if !t.contains_key(&cap) { t.insert(cap, name.to_vec()); }
}

fn file_name_forget(cap: usize) { FILE_NAMES.lock().remove(&cap); }

/// mm file-name hook: `cap`'s path into `out`, 0 if unknown.
fn exec_file_name(cap: usize, out: &mut [u8]) -> usize {
    let t = FILE_NAMES.lock();
    match t.get(&cap) {
        Some(n) => { let k = n.len().min(out.len()); out[..k].copy_from_slice(&n[..k]); k }
        None => 0,
    }
}

/// Record the path of `fd` as the name of `cap` (first mapping of a file).
fn file_name_from_fd(cap: usize, pid: u32, fd: usize) {
    if file_name_known(cap) { return; }
    let mut kb: Vec<u8> = alloc::vec![0u8; HOST_PATH_MAX];
    let msg = make_vfs_msg(vfs::VFS_FD_PATH, &[fd as u64, kb.as_mut_ptr() as u64, HOST_PATH_MAX as u64]);
    let n = vfs_reply_val(&vfs::handle(&msg, pid));
    if n > 0 { file_name_note(cap, &kb[..(n as usize).min(HOST_PATH_MAX)]); }
}

/// The registered cap of inode (port, ino) with a reference taken, if
/// anything maps it (`mmap_file_ref(cap, false)` releases it).
fn mmap_cap_get(port: u32, ino: u32) -> Option<usize> {
    let mut tbl = MMAP_FILES.lock();
    let i = *tbl.by_key.get(&(port, ino))?;
    tbl.v.get_mut(i)?.as_mut()?.refs += 1;
    Some(MMAP_CAP_BASE + i)
}

/// Wire the mm crate's file-backed-VMA hooks to the registry above.
/// Called once from kernel init, before userspace starts.
pub fn init_exec_file_backing() {
    mm::vmm::set_file_backing_hooks(exec_file_read, exec_file_retain, exec_file_release);
    mm::vmm::set_device_vma_hooks(drivers::drm_device_interface::hostvis_vma_retain,
                                  drivers::drm_device_interface::hostvis_vma_release);
    mm::vmm::set_file_key_hook(exec_file_key);
    mm::vmm::set_vmo_page_hook(vfs::vmo_fault_frames);
    mm::vmm::set_vmo_ref_hooks(vfs::vmo_retain, vfs::vmo_release);
    mm::vmm::set_file_name_hook(exec_file_name);
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
/// Userspace may write all of `[ptr, ptr+len)`. Checked before a buffer the
/// kernel fills goes to VFS/f2fs: a store into a read-only mapping there
/// would fault with the filesystem lock held (the task was killed in place
/// and the machine hung); Linux answers EFAULT.
fn user_buf_writable(ptr: usize, len: usize) -> bool {
    with_current_address_space(|as_| as_.range_writable(ptr, len)).unwrap_or(false)
}

/// Copy `src` to the current task's user address `dst` through the user
/// mapping, faulting absent pages in (demand-zero, file-backed, CoW) like the
/// task's own store would. False for a range no access can make valid
/// (unmapped, read-only, PROT_NONE): the caller answers EFAULT. Must not be
/// called with a filesystem lock held (a file-backed fault reads the file).
fn copy_to_user_bytes(dst: usize, src: &[u8]) -> bool {
    unsafe { ua::copy_to_user(dst, src.as_ptr(), src.len()) == 0 }
}

fn prefault_user(ptr: usize, len: usize) {
    // Absent file-backed pages (exec image, private mmap) are read with the
    // address space unlocked; the locked walk does the rest (anonymous
    // pages, CoW unsharing of present pages). See sched::prefault_current_range.
    sched::prefault_current_range(ptr, len, false);
}

/// [`prefault_user`] for a buffer the kernel only reads (paths, execve's
/// argv/envp): faults absent pages in, but does not unshare copy-on-write
/// pages — reading one never faults, so the private copy (and its TLB flush)
/// would be wasted.
fn prefault_user_ro(ptr: usize, len: usize) {
    sched::prefault_current_range(ptr, len, true);
}

/// Bytes of a read/write buffer worth prefaulting for `fd`: a pipe read
/// delivers at most PIPE_READ_MAX and a pipe write accepts at most the ring,
/// so prefaulting a 1 MiB buffer per call is pure overhead (256 pages x VMA scans).
fn pipe_prefault_len(pid: u32, fd: usize, count: usize, write: bool) -> usize {
    if vfs::is_handle(fd) { return count; }
    match vfs::pipe_end_of(pid, fd) {
        Some(_) => count.min(if write { vfs::PIPE_RING_SIZE } else { vfs::PIPE_READ_MAX }),
        None => count,
    }
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
/// Size of the initial user stack mapping (8 MiB — matches the Linux default
/// main-thread stack `ulimit -s`). The prior 256 KiB was far too small for real
/// Rust async runtimes (tokio + zbus): legitimate deep async call chains can
/// exceed it, faulting exactly one frame below the stack base. The stack is
/// demand-paged (see execve), so only the touched part is resident. (Note: a
/// truly *unbounded*-recursion bug still overflows any finite stack — see the
/// cosmic-comp COSMIC_SESSION_SOCK issue — so this is a correctness/robustness
/// raise, not a fix for that.)
const USER_STACK_SIZE: usize = 2048 * mm::buddy::PAGE_SIZE;
/// Main-thread stack VMA of a new image: the caller's RLIMIT_STACK soft
/// limit, page-rounded and clamped to [STACK_MIN, STACK_MAX]; the 8 MiB
/// default (Linux's `ulimit -s`) when unlimited. The VMA does not grow, so
/// this is where a deep recursion meets SIGSEGV. A limit lowered after exec
/// applies from the next exec on (Linux applies it at the next growth).
fn exec_stack_size() -> usize {
    const STACK_MIN: u64 = 128 * 1024;
    const STACK_MAX: u64 = 4 << 30;
    let lim = sched::current_rlimit(sched::task::RLIMIT_STACK);
    if lim == sched::task::RLIM_INFINITY { return USER_STACK_SIZE; }
    let page = mm::buddy::PAGE_SIZE as u64;
    (((lim + page - 1) & !(page - 1)).clamp(STACK_MIN, STACK_MAX)) as usize
}

/// RLIMIT_AS / RLIMIT_DATA admission for `grow` more bytes of mappings
/// (`data`: the new bytes are private and writable, counted in VmData as
/// Linux counts them). False when the address space would exceed a limit:
/// the caller answers ENOMEM (brk: the old break). The totals are only
/// computed when one of the limits is finite.
fn vm_limits_admit(grow: usize, data: bool) -> bool {
    use sched::task::{RLIMIT_AS, RLIMIT_DATA, RLIM_INFINITY};
    let as_lim = sched::current_rlimit(RLIMIT_AS);
    let data_lim = if data { sched::current_rlimit(RLIMIT_DATA) } else { RLIM_INFINITY };
    if as_lim == RLIM_INFINITY && data_lim == RLIM_INFINITY { return true; }
    let mc = match with_current_address_space(|a| a.mem_counters()) { Some(m) => m, None => return true };
    let page = mm::buddy::PAGE_SIZE as u64;
    let grow = grow as u64;
    if as_lim != RLIM_INFINITY && (mc.size as u64 * page).saturating_add(grow) > as_lim { return false; }
    if data_lim != RLIM_INFINITY && (mc.data as u64 * page).saturating_add(grow) > data_lim { return false; }
    true
}

/// Stack populated below the initial SP at exec (demand-paged beyond that).
const STACK_PREFAULT: usize = 4 * mm::buddy::PAGE_SIZE;

/// ELF `e_type` for a position-independent (dynamic/PIE) object.
const ET_DYN: u16 = 3;
/// Load bias for an `ET_DYN` **main** executable (static-PIE or dynamic-PIE).
/// Placed at 2 MiB — identical to the fixed address `ET_EXEC` images have
/// always loaded at, so the image + brk heap layout is unchanged and back-
/// compatibility with the existing static userland is preserved by construction.
const MAIN_DYN_BASE: usize = 0x0020_0000;
/// Load bias for the dynamic interpreter (`/lib/ld-musl-<arch>.so.1`).  Sits in
/// the free window at 768 MiB — above any realistic brk heap growing up from
/// the ~2 MiB main image, and 256 MiB below the `mmap` window at 1 GiB
/// (`mm::vmm::MMAP_BASE`) where ld.so's own library mappings land.
const INTERP_BASE: usize = 0x3000_0000;

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
/// Registry name of block device `index` (`vda`, `sda2`) into a 16-byte
/// buffer; returns its length. Lets `lsblk` print partitions and USB disks
/// under their real names instead of deriving `vd<letter>` from the index.
pub const SYS_BLKDEV_NAME:  usize = 523;

/// Gated debug: emit a short user string straight to the serial TX, framed, so
/// a userspace checkpoint round-trips even when a compositor owns the console
/// (PL011 RX starves under HVF but TX is always reliable). See DBG_SERIAL_WRITE.
pub const SYS_DBG_SERIAL_WRITE: usize = 590;
/// Root-only hardening probe for privtest (`sys_secprobe`).
pub const SYS_SECPROBE: usize = 591;

// ── AArch64 Linux syscall numbers ─────────────────────────────────────────────
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const RESTART_SYSCALL: usize = 128;
    pub const SET_ROBUST_LIST: usize = 99;
    pub const GET_ROBUST_LIST: usize = 100;
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
    pub const RECVMMSG:       usize = 243;
    pub const SENDMMSG:       usize = 269;
    pub const SOCKETPAIR:     usize = 199;
    pub const CLOCK_NANOSLEEP: usize = 115;
    pub const NANOSLEEP:      usize = 101;
    pub const GETTIMEOFDAY:   usize = 169;
    pub const SETTIMEOFDAY:   usize = 170;
    pub const CLOCK_SETTIME:  usize = 112;
    pub const SYSINFO:        usize = 179;
    pub const GETRLIMIT:      usize = 163;
    pub const SETRLIMIT:      usize = 164;
    pub const SENDFILE:       usize = 71;
    pub const SETITIMER:      usize = 103;
    pub const GETITIMER:      usize = 102;
    pub const SIGPENDING:     usize = 136;
    pub const GETRANDOM:      usize = 278;
    pub const SECCOMP:        usize = 277;
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
    pub const PREADV:         usize = 69;
    pub const PWRITEV:        usize = 70;
    pub const PREADV2:        usize = 286;
    pub const PWRITEV2:       usize = 287;
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
    pub const CHROOT:         usize = 51;
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
    pub const SETFSUID:       usize = 151;
    pub const SETFSGID:       usize = 152;
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
    pub const FACCESSAT2:     usize = 439;
    pub const FCHMODAT2:      usize = 452;
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
    pub const TEE:                 usize = 77;
    pub const VMSPLICE:            usize = 75;
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
    // Note the AArch64/asm-generic ordering is the reverse of x86-64's.
    pub const SETPRIORITY:         usize = 140;
    pub const GETPRIORITY:         usize = 141;
    pub const CAPGET:              usize = 90;
    pub const CAPSET:              usize = 91;
    pub const MEMBARRIER:          usize = 283;
    pub const MBIND:               usize = 235;
    pub const GET_MEMPOLICY:       usize = 236;
    pub const SET_MEMPOLICY:       usize = 237;
    pub const RSEQ:                usize = 293;
    pub const STATX:               usize = 291;
    pub const OPENAT2:             usize = 437;
    pub const CLONE3:              usize = 435;
    pub const CLOSE_RANGE:         usize = 436;
    pub const UNSHARE:             usize = 97;
    pub const PIDFD_OPEN:          usize = 434;
    pub const PIDFD_SEND_SIGNAL:   usize = 424;
    pub const PIDFD_GETFD:         usize = 438;
    pub const RT_SIGTIMEDWAIT:     usize = 137;
    // AArch64 asm-generic: inotify_init1 = 26 (was mistakenly 360, which only
    // ever mattered once inotify stopped being ENOSYS in K2).
    pub const INOTIFY_INIT1:       usize = 26;
    pub const INOTIFY_ADD_WATCH:   usize = 27;
    pub const INOTIFY_RM_WATCH:    usize = 28;
    pub const POSIX_FADVISE:       usize = 223;
    pub const SYNC_FILE_RANGE:     usize = 84;
    pub const SYNC:                usize = 81;
    pub const REBOOT:              usize = 142;
    pub const SYNCFS:              usize = 267;
    pub const READAHEAD:           usize = 213;
    pub const GETCPU:              usize = 168;
    pub const UMOUNT2:             usize = 39;
    pub const MOUNT:               usize = 40;
    pub const PIVOT_ROOT:          usize = 41;
    // Extended attributes (asm-generic 5..=16)
    pub const SETXATTR:            usize = 5;
    pub const LSETXATTR:           usize = 6;
    pub const FSETXATTR:           usize = 7;
    pub const GETXATTR:            usize = 8;
    pub const LGETXATTR:           usize = 9;
    pub const FGETXATTR:           usize = 10;
    pub const LISTXATTR:           usize = 11;
    pub const LLISTXATTR:          usize = 12;
    pub const FLISTXATTR:          usize = 13;
    pub const REMOVEXATTR:         usize = 14;
    pub const LREMOVEXATTR:        usize = 15;
    pub const FREMOVEXATTR:        usize = 16;
    // Privileged-or-credential syscalls (see "Privilege" in dispatch_inner).
    pub const SETREGID:            usize = 143;
    pub const SETREUID:            usize = 145;
    pub const SETHOSTNAME:         usize = 161;
    pub const SETDOMAINNAME:       usize = 162;
    pub const RT_SIGQUEUEINFO:     usize = 138;
    pub const RT_TGSIGQUEUEINFO:   usize = 240;
    pub const ACCT:                usize = 89;
    pub const VHANGUP:             usize = 58;
    pub const SWAPON:              usize = 224;
    pub const SWAPOFF:             usize = 225;
    pub const INIT_MODULE:         usize = 105;
    pub const DELETE_MODULE:       usize = 106;
    pub const FINIT_MODULE:        usize = 273;
    pub const KEXEC_LOAD:          usize = 104;
    pub const KEXEC_FILE_LOAD:     usize = 294;
}

// ── x86-64 Linux syscall numbers ──────────────────────────────────────────────
#[cfg(not(target_arch = "aarch64"))]
mod nr {
    pub const RESTART_SYSCALL: usize = 219;
    pub const SET_ROBUST_LIST: usize = 273;
    pub const GET_ROBUST_LIST: usize = 274;
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
    pub const VFORK:          usize = 58;
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
    pub const RECVMMSG:       usize = 299;
    pub const SENDMMSG:       usize = 307;
    pub const CLOCK_NANOSLEEP: usize = 230;
    pub const NANOSLEEP:      usize = 35;
    pub const GETTIMEOFDAY:   usize = 96;
    pub const SETTIMEOFDAY:   usize = 164;
    pub const CLOCK_SETTIME:  usize = 227;
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
    pub const SECCOMP:        usize = 317;
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
    pub const PREADV:         usize = 295;
    pub const PWRITEV:        usize = 296;
    pub const PREADV2:        usize = 327;
    pub const PWRITEV2:       usize = 328;
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
    pub const CHROOT:         usize = 161;
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
    pub const SETFSUID:       usize = 122;
    pub const SETFSGID:       usize = 123;
    pub const GETRESUID:      usize = 118;
    pub const GETRESGID:      usize = 120;
    pub const UMASK:          usize = 95;
    pub const GETGROUPS:      usize = 115;
    pub const SETGROUPS:      usize = 116;
    // Filesystem operations
    pub const DUP2:           usize = 33;
    pub const MKDIRAT:        usize = 258;
    pub const MKDIR:          usize = 83;
    pub const RMDIR:          usize = 84;
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
    pub const FACCESSAT2:     usize = 439;
    pub const FCHMODAT2:      usize = 452;
    pub const STATFS:         usize = 137;
    pub const FSTATFS:        usize = 138;
    pub const FSYNC:          usize = 74;
    pub const FDATASYNC:      usize = 75;
    pub const FALLOCATE:      usize = 285;
    pub const UTIMENSAT:      usize = 280;
    pub const UTIME:          usize = 132;
    pub const UTIMES:         usize = 235;
    pub const FUTIMESAT:      usize = 261;
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
    pub const TEE:                 usize = 276;
    pub const VMSPLICE:            usize = 278;
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
    // Reversed relative to AArch64 — see the note there. There is no `nice`
    // syscall on x86-64 (34 is `pause`); it is i386-only, and musl implements
    // nice(3) on top of get/setpriority.
    pub const GETPRIORITY:         usize = 140;
    pub const SETPRIORITY:         usize = 141;
    pub const CAPGET:              usize = 125;
    pub const CAPSET:              usize = 126;
    pub const MEMBARRIER:          usize = 324;
    pub const MBIND:               usize = 237;
    pub const SET_MEMPOLICY:       usize = 238;
    pub const GET_MEMPOLICY:       usize = 239;
    pub const RSEQ:                usize = 334;
    pub const STATX:               usize = 332;
    pub const OPENAT2:             usize = 437;
    pub const CLONE3:              usize = 435;
    pub const CLOSE_RANGE:         usize = 436;
    pub const UNSHARE:             usize = 272;
    pub const PIDFD_OPEN:          usize = 434;
    pub const PIDFD_SEND_SIGNAL:   usize = 424;
    pub const PIDFD_GETFD:         usize = 438;
    pub const RT_SIGTIMEDWAIT:     usize = 128;
    pub const INOTIFY_INIT1:       usize = 294;
    pub const INOTIFY_ADD_WATCH:   usize = 254;
    pub const INOTIFY_RM_WATCH:    usize = 255;
    pub const POSIX_FADVISE:       usize = 221;
    pub const SYNC_FILE_RANGE:     usize = 277;
    pub const SYNC:                usize = 162;
    pub const REBOOT:              usize = 169;
    pub const SYNCFS:              usize = 306;
    pub const READAHEAD:           usize = 187;
    pub const GETCPU:              usize = 309;
    pub const MOUNT:               usize = 165;
    pub const UMOUNT2:             usize = 166;
    pub const PIVOT_ROOT:          usize = 155;
    // Extended attributes (x86-64 188..=199)
    pub const SETXATTR:            usize = 188;
    pub const LSETXATTR:           usize = 189;
    pub const FSETXATTR:           usize = 190;
    pub const GETXATTR:            usize = 191;
    pub const LGETXATTR:           usize = 192;
    pub const FGETXATTR:           usize = 193;
    pub const LISTXATTR:           usize = 194;
    pub const LLISTXATTR:          usize = 195;
    pub const FLISTXATTR:          usize = 196;
    pub const REMOVEXATTR:         usize = 197;
    pub const LREMOVEXATTR:        usize = 198;
    pub const FREMOVEXATTR:        usize = 199;
    // Privileged-or-credential syscalls (see "Privilege" in dispatch_inner).
    pub const SETREUID:            usize = 113;
    pub const SETREGID:            usize = 114;
    pub const SETHOSTNAME:         usize = 170;
    pub const SETDOMAINNAME:       usize = 171;
    pub const RT_SIGQUEUEINFO:     usize = 129;
    pub const RT_TGSIGQUEUEINFO:   usize = 297;
    pub const ACCT:                usize = 163;
    pub const VHANGUP:             usize = 153;
    pub const SWAPON:              usize = 167;
    pub const SWAPOFF:             usize = 168;
    pub const INIT_MODULE:         usize = 175;
    pub const DELETE_MODULE:       usize = 176;
    pub const FINIT_MODULE:        usize = 313;
    pub const KEXEC_LOAD:          usize = 246;
    pub const KEXEC_FILE_LOAD:     usize = 320;
    pub const IOPL:                usize = 172;
    pub const IOPERM:              usize = 173;
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
    // IF=0 here on x86: answer a pending TLB shootdown now rather than
    // leaving its initiator spinning for the length of this syscall.
    mm::paging::tlb_service_pending();
    // seccomp: per-CPU pointer + one Acquire load when the thread has no filter.
    if sched::current_seccomp().is_some() {
        let ip = if frame_ptr == 0 { 0 } else {
            let uf = unsafe { &*(frame_ptr as *const sched::context::UserFrame) };
            #[cfg(target_arch = "x86_64")] { uf.rip }
            #[cfg(target_arch = "aarch64")] { uf.elr_el1 }
        };
        if let Some(r) = sched::seccomp::on_syscall(number, [a0, a1, a2, a3, a4, a5], ip) {
            return r;
        }
    }
    let ret = dispatch(number, a0, a1, a2, a3, a4, a5, frame_ptr);
    if number != RT_SIGRETURN && (ret == ERESTARTSYS || ret == ERESTARTNOHAND) {
        // Never reaches user space: the frame gets EINTR, and the signal
        // pass that runs next rewinds to re-execute the syscall when no
        // handler runs, or (ERESTARTSYS only) when the handler has SA_RESTART.
        sched::signal::note_syscall_restart(number, a0, ret == ERESTARTSYS);
        return -4;
    }
    if number != RT_SIGRETURN && ret == ERESTART_RESTARTBLOCK {
        // Same rule as ERESTARTNOHAND, but what gets re-executed is
        // restart_syscall(2), which resumes this thread's `restart_block`
        // (the original absolute deadline) rather than the original call
        // with its relative timeout re-read from scratch.
        sched::signal::note_syscall_restart(RESTART_SYSCALL, a0, false);
        return -4;
    }
    ret
}

/// Kernel-internal "interrupted, restartable" return (Linux -ERESTARTSYS):
/// used by blocking read/write/send/recv/wait4/waitid. Timed waits
/// (nanosleep, poll, select, epoll_wait, sigsuspend, sigtimedwait) return a
/// plain EINTR, as on Linux, whatever SA_RESTART says.
const ERESTARTSYS: isize = -512;
/// Linux -ERESTARTNOHAND: restarted only when no handler runs (a stop and
/// continue, or a signal another thread took); EINTR whenever a handler runs,
/// SA_RESTART or not. Used by timed FUTEX_WAIT, which Linux answers with
/// -ERESTART_RESTARTBLOCK — the same user-visible rule.
const ERESTARTNOHAND: isize = -514;
/// Linux -ERESTART_RESTARTBLOCK: restarted (as `restart_syscall`) only when no
/// handler runs, EINTR otherwise. The interrupted call first stores what to
/// resume — for every user here, its absolute deadline — with
/// `sched::set_restart_block`, so a relative timeout is not re-armed in full
/// on the restart (Linux: `hrtimer_nanosleep_restart`, `futex_wait_restart`,
/// `do_restart_poll`).
const ERESTART_RESTARTBLOCK: isize = -516;

/// `restart_block[0]` kinds (0 = nothing to resume).
const RB_FUTEX:     u64 = 1; // [uaddr, val, private, deadline, realtime?, realtime deadline]
const RB_NANOSLEEP: u64 = 2; // [deadline, rmtp]
const RB_POLL:      u64 = 3; // [fds_ptr, nfds, deadline (u64::MAX = none), sigmask, wb]
const RB_SELECT:    u64 = 4; // [nfds, rfds, wfds, efds, deadline (u64::MAX = none), sigmask, wb]

/// ppoll/pselect6 with a NULL sigmask (a real one never has SIGKILL set).
const NO_SIGMASK: u64 = u64::MAX;
/// SIGKILL and SIGSTOP: dropped from any mask user space installs.
const SIG_UNBLOCKABLE: u64 = (1 << 8) | (1 << 18);
/// Timeout write-back word: the user pointer, with this bit set when it is a
/// `struct timeval` (select) rather than a `struct timespec`; 0 = none.
const WB_TIMEVAL: u64 = 1 << 63;

/// restart_syscall(2): resume the calling thread's `restart_block`. Only ever
/// reached through the rewind that ERESTART_RESTARTBLOCK sets up; with
/// nothing stored (a direct call) it answers EINTR, as Linux's
/// `do_no_restart_syscall` does.
fn sys_restart_syscall() -> isize {
    let b = sched::take_restart_block();
    match b[0] {
        RB_FUTEX => {
            let (uaddr, val) = (b[1] as usize, b[2] as u32);
            if !validate_user_ptr_aligned(uaddr, 4, 4) { return -14; }
            // A CLOCK_REALTIME FUTEX_WAIT_BITSET deadline is an instant on
            // the wall clock: translate it again, so a clock step while the
            // waiter was stopped moves the deadline (Linux restarts it as
            // an absolute CLOCK_REALTIME hrtimer).
            let rt = (b[5] != 0).then_some(b[6]);
            let deadline = match rt { Some(r) => sched::realtime_to_monotonic_ns(r), None => b[4] };
            if monotonic_ns() >= deadline {
                // Deadline passed while stopped: the value check still comes
                // first, as for an expired FUTEX_WAIT_BITSET.
                let cur = ur::<u32>(uaddr);
                return if cur != val { -11 } else { -110 }; // EAGAIN / ETIMEDOUT
            }
            futex_wait_timed(uaddr, val, b[3] != 0, deadline, rt)
        }
        RB_NANOSLEEP => {
            if monotonic_ns() >= b[1] { return 0; }
            sleep_until_ns(b[1], b[2] as usize, true)
        }
        RB_POLL => {
            let nfds = b[2] as usize;
            if nfds != 0 && !validate_user_buf(b[1] as usize, nfds.saturating_mul(8)) { return -14; }
            poll_masked(b[1] as usize, nfds, b[3], b[4], b[5])
        }
        RB_SELECT => select_masked(b[1] as usize, b[2] as usize, b[3] as usize, b[4] as usize, b[5], b[6], b[7]),
        _ => -4,
    }
}

/// zink-lane instrumentation: per-syscall time census for the focus tgid
/// (`sched::SC_FOCUS_TGID`, cosmic-comp) plus a lock-free pid -> last syscall
/// table, printed by `scstat_tick`. Compile-time gated like `DRM_STATS`.
pub const SC_STATS: bool = false;
const SC_SLOTS: usize = 512;
/// Index 0 follows `SC_FOCUS_TGID` (cosmic-comp), 1 `SC_FOCUS2_TGID`
/// (cosmic-greeter-login).
static SC_N:   [[AtomicU64; SC_SLOTS]; 2] = [const { [const { AtomicU64::new(0) }; SC_SLOTS] }; 2];
static SC_NS:  [[AtomicU64; SC_SLOTS]; 2] = [const { [const { AtomicU64::new(0) }; SC_SLOTS] }; 2];
static SC_MAX: [[AtomicU64; SC_SLOTS]; 2] = [const { [const { AtomicU64::new(0) }; SC_SLOTS] }; 2];
/// pid & 1023 -> (pid << 32) | (nr << 1) | in_syscall
static LAST_SC: [AtomicU64; 1024] = [const { AtomicU64::new(0) }; 1024];
/// 0 = not requested, 1 = dump the focus process's VMAs on its next syscall, 2 = done.
static SC_VMA_DUMP: AtomicU32 = AtomicU32::new(0);

/// compchurn instrumentation: mmap/munmap census keyed by (tgid, pid, op,
/// kind, len, flags), printed every 10 s as `[CHURN]` lines (top rows by
/// count, then reset). `op` 0 = mmap, 1 = munmap. mmap `kind` is
/// `MmapTrace::kind`; munmap `kind` names the VMA at `addr` (1 anon, 2 device,
/// 3 shared frames, 5 file, 0 none) and `pres` its present pages. `ns` is the
/// syscall's own time. Compile-time gated like `SC_STATS`.
pub const CHURN_STATS: bool = false;
#[derive(Clone, Copy)]
struct ChurnRow { tgid: u32, pid: u32, op: u8, kind: u8, len: usize, flags: u32, n: u32, ns: u64, max_ns: u64, pres: u64 }
const CHURN_ROWS: usize = 96;
static CHURN: spin::Mutex<[Option<ChurnRow>; CHURN_ROWS]> = spin::Mutex::new([None; CHURN_ROWS]);
static CHURN_DROPPED: AtomicU64 = AtomicU64::new(0);

fn churn_note(op: u8, kind: u8, len: usize, flags: u32, ns: u64, pres: u64) {
    if !CHURN_STATS { return; }
    let pid = current_pid();
    let tgid = sched::current_tgid();
    let mut t = CHURN.lock();
    let mut free = None;
    for (i, r) in t.iter_mut().enumerate() {
        match r {
            Some(r) if r.tgid == tgid && r.pid == pid && r.op == op && r.kind == kind
                && r.len == len && r.flags == flags => {
                r.n += 1; r.ns += ns; r.max_ns = r.max_ns.max(ns); r.pres += pres;
                return;
            }
            None if free.is_none() => free = Some(i),
            _ => {}
        }
    }
    match free {
        Some(i) => t[i] = Some(ChurnRow { tgid, pid, op, kind, len, flags, n: 1, ns, max_ns: ns, pres }),
        None => { CHURN_DROPPED.fetch_add(1, Ordering::Relaxed); }
    }
}

fn churn_tick() {
    use core::sync::atomic::Ordering::Relaxed;
    if !CHURN_STATS { return; }
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = monotonic_ns();
    let last = LAST.load(Relaxed);
    if last == 0 { LAST.store(now, Relaxed); return; }
    if now.wrapping_sub(last) < 10_000_000_000 { return; }
    let rows = match CHURN.try_lock() {
        Some(mut t) => { let r = *t; *t = [None; CHURN_ROWS]; r }
        None => return,
    };
    LAST.store(now, Relaxed);
    let win_ms = (now.wrapping_sub(last) / 1_000_000) as usize;
    mm::gap2::s("[CHURN] win_ms="); mm::gap2::h(win_ms);
    mm::gap2::kv(" dropped=", CHURN_DROPPED.swap(0, Relaxed) as usize);
    mm::gap2::nl();
    mm::gap2::s("[CHURN] alltag sock="); mm::gap2::h(ALLTAG[0].swap(0, Relaxed) as usize);
    mm::gap2::kv(" epoll=", ALLTAG[1].swap(0, Relaxed) as usize);
    mm::gap2::kv(" signalfd=", ALLTAG[2].swap(0, Relaxed) as usize);
    mm::gap2::kv(" inotify=", ALLTAG[3].swap(0, Relaxed) as usize);
    mm::gap2::kv(" dev=", ALLTAG[4].swap(0, Relaxed) as usize);
    mm::gap2::kv(" stdio=", ALLTAG[5].swap(0, Relaxed) as usize);
    mm::gap2::kv(" file=", ALLTAG[6].swap(0, Relaxed) as usize);
    mm::gap2::kv(" other=", ALLTAG[7].swap(0, Relaxed) as usize);
    mm::gap2::nl();
    let mut taken = [false; CHURN_ROWS];
    for _ in 0..24 {
        let mut best = usize::MAX; let mut bn = 0u32;
        for (i, r) in rows.iter().enumerate() {
            if taken[i] { continue; }
            if let Some(r) = r { if r.n > bn { bn = r.n; best = i; } }
        }
        if best == usize::MAX { break; }
        taken[best] = true;
        let r = rows[best].unwrap();
        mm::gap2::s(if r.op == 0 { "[CHURN] mmap" } else { "[CHURN] munmap" });
        mm::gap2::kv(" tgid=", r.tgid as usize);
        mm::gap2::kv(" pid=", r.pid as usize);
        mm::gap2::kv(" kind=", r.kind as usize);
        mm::gap2::kv(" len=", r.len);
        mm::gap2::kv(" flags=", r.flags as usize);
        mm::gap2::kv(" n=", r.n as usize);
        mm::gap2::kv(" avg_ns=", (r.ns / r.n.max(1) as u64) as usize);
        mm::gap2::kv(" max_ns=", r.max_ns as usize);
        mm::gap2::kv(" pres=", r.pres as usize);
        mm::gap2::nl();
    }
}

/// greeterdisp instrumentation: what an idle event loop is actually doing.
/// Per thread: epoll_wait/poll calls and how each ended (events, timeout,
/// EINTR), zero-timeout and short (<= 2 ms) timeouts, parks, and SPURIOUS
/// wakes (woken, re-probed, nothing ready, parked again — see
/// `sched::gdwake`); EAGAIN from read/recv (a poll that said "readable" with
/// nothing to read); total syscalls. Per (tgid, fd): events delivered. Per
/// (waker site, tgid): useful vs spurious wakes. Printed every 10 s as
/// `[GDT]`/`[GDFD]`/`[GDSITE]`. Compile-time gated with `sched::gdwake`.
#[inline(always)]
pub fn gd_stats() -> bool { sched::gdwake::enabled() }
/// Print the per-thread `[GDWIN]/[GDT]/[GDFD]/[GDSC]` tables to serial every
/// 10 s while `sched.wakestat=1` is on. Off by default: the per-site
/// useful/spurious table is what `/proc/wakestat` serves, and the serial
/// flood would distort the very wake pattern being measured.
const GD_SERIAL: bool = false;
const GD_K: usize = 24;
const K_EP_CALL: usize = 0; const K_EP_EV: usize = 1; const K_EP_TMO: usize = 2;
const K_EP_INTR: usize = 3; const K_EP_SPUR: usize = 4; const K_EP_ZERO: usize = 5;
const K_EP_SHORT: usize = 6; const K_EP_PARK: usize = 7;
const K_PO_CALL: usize = 8; const K_PO_EV: usize = 9; const K_PO_TMO: usize = 10;
const K_PO_SPUR: usize = 11; const K_PO_ZERO: usize = 12; const K_PO_SHORT: usize = 13;
const K_PO_PARK: usize = 14; const K_EAGAIN: usize = 15; const K_SC: usize = 16;
const K_USEFUL: usize = 17; const K_PARKALL: usize = 18;
const K_FXW_OK: usize = 19; const K_FXW_TMO: usize = 20; const K_FXW_AGAIN: usize = 21;
const K_FX_WAKE: usize = 22; const K_FX_WOKEN: usize = 23;
const GD_NAMES: [&str; GD_K] = ["ep", "ep_ev", "ep_tmo", "ep_intr", "ep_spur", "ep_zero",
    "ep_short", "ep_park", "po", "po_ev", "po_tmo", "po_spur", "po_zero", "po_short",
    "po_park", "eagain", "sc", "useful", "park_all", "fxw_ok", "fxw_tmo", "fxw_again",
    "fx_wake", "fx_woken"];
static GD_PID: [AtomicU32; 1024] = [const { AtomicU32::new(0) }; 1024];
static GD_TGID: [AtomicU32; 1024] = [const { AtomicU32::new(0) }; 1024];
static GD_C: [[AtomicU32; GD_K]; 1024] = [const { [const { AtomicU32::new(0) }; GD_K] }; 1024];
const GD_FDS: usize = 128;
static GD_FD_KEY: [AtomicU64; GD_FDS] = [const { AtomicU64::new(0) }; GD_FDS];
static GD_FD_N: [AtomicU32; GD_FDS] = [const { AtomicU32::new(0) }; GD_FDS];
/// kind << 24 | index: 1 pipe, 2 pty, 3 eventfd, 4 timerfd, 5 device (dev_id),
/// 6 signalfd, 7 inotify, 8 file, 9 other vfs, 10 socket, 11 epoll.
static GD_FD_KIND: [AtomicU32; GD_FDS] = [const { AtomicU32::new(0) }; GD_FDS];
/// Syscalls by (pid, nr): key = pid << 16 | nr.
const GD_SCN: usize = 256;
static GD_SCN_KEY: [AtomicU64; GD_SCN] = [const { AtomicU64::new(0) }; GD_SCN];
static GD_SCN_N: [AtomicU32; GD_SCN] = [const { AtomicU32::new(0) }; GD_SCN];

fn gd_scn(nr: usize) {
    let key = ((current_pid() as u64) << 16) | (nr as u64 & 0xFFFF);
    let h = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize % GD_SCN;
    for k in 0..GD_SCN {
        let i = (h + k) % GD_SCN;
        let cur = GD_SCN_KEY[i].load(Ordering::Relaxed);
        if cur == key || (cur == 0 && GD_SCN_KEY[i].compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed)
            .map_or_else(|v| v == key, |_| true)) {
            GD_SCN_N[i].fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

fn gd_fd_kind(pid: u32, fd: usize) -> u32 {
    if fd >= EPOLL_FD_BASE { return 11 << 24; }
    if fd >= net_server::SOCK_FD_BASE { return 10 << 24; }
    match vfs::vfs_get_node_kind(pid, fd) {
        Some(vfs::VnodeKind::Pipe { ring, .. }) => (1 << 24) | ring as u32,
        Some(vfs::VnodeKind::Pty { pair, .. }) => (2 << 24) | pair as u32,
        Some(vfs::VnodeKind::EventFd { slot }) => (3 << 24) | slot as u32,
        Some(vfs::VnodeKind::TimerFd { slot }) => (4 << 24) | slot as u32,
        Some(vfs::VnodeKind::DynamicDevice { dev_id, .. }) => (5 << 24) | dev_id,
        Some(vfs::VnodeKind::SignalFd { .. }) => 6 << 24,
        Some(vfs::VnodeKind::Inotify { .. }) => 7 << 24,
        Some(vfs::VnodeKind::MountedFile { .. }) | Some(vfs::VnodeKind::TmpFile { .. })
            | Some(vfs::VnodeKind::RamFile { .. }) => 8 << 24,
        _ => 9 << 24,
    }
}
#[inline]
fn gd_inc(k: usize) {
    if !gd_stats() { return; }
    let pid = current_pid();
    let s = pid as usize & 1023;
    if GD_PID[s].load(Ordering::Relaxed) != pid {
        GD_PID[s].store(pid, Ordering::Relaxed);
        for c in GD_C[s].iter() { c.store(0, Ordering::Relaxed); }
    }
    GD_TGID[s].store(sched::current_tgid(), Ordering::Relaxed);
    GD_C[s][k].fetch_add(1, Ordering::Relaxed);
}

/// Events delivered on `fd` of the current thread group (`kind` 0 = epoll,
/// 1 = poll).
fn gd_fd(fd: usize, kind: u64) {
    if !gd_stats() { return; }
    let key = ((sched::current_tgid() as u64) << 32) | ((fd as u64 & 0xFFFF) << 1) | kind;
    let h = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 57) as usize % GD_FDS;
    for k in 0..GD_FDS {
        let i = (h + k) % GD_FDS;
        let cur = GD_FD_KEY[i].load(Ordering::Relaxed);
        if cur == key || (cur == 0 && GD_FD_KEY[i].compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed)
            .map_or_else(|v| v == key, |_| true)) {
            if GD_FD_N[i].fetch_add(1, Ordering::Relaxed) == 0 || GD_FD_KIND[i].load(Ordering::Relaxed) == 0 {
                GD_FD_KIND[i].store(gd_fd_kind(current_pid(), fd), Ordering::Relaxed);
            }
            return;
        }
    }
}

/// Classify the wake that ended the last park: `useful` = the loop then found
/// something to report (or timed out, for an own-deadline wake).
fn gd_site(site: u64, useful: bool, poll: bool) {
    if !gd_stats() || site == 0 { return; }
    if !useful { gd_inc(if poll { K_PO_SPUR } else { K_EP_SPUR }); } else { gd_inc(K_USEFUL); }
    sched::gdwake::note_outcome(site, sched::current_tgid(), useful);
}

fn gp(m: &str) { if GD_SERIAL { mm::gap2::s(m) } }
fn gpn() { if GD_SERIAL { mm::gap2::nl() } }

fn gd_dec(mut n: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; if n == 0 { break; } }
    gp(core::str::from_utf8(&buf[i..]).unwrap_or("?"));
}

fn gd_tick() {
    use core::sync::atomic::Ordering::Relaxed;
    if !gd_stats() { return; }
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = monotonic_ns();
    let last = LAST.load(Relaxed);
    if last == 0 { LAST.store(now, Relaxed); return; }
    if now.wrapping_sub(last) < 10_000_000_000 { return; }
    LAST.store(now, Relaxed);
    gp("[GDWIN] ms="); gd_dec((now.wrapping_sub(last) / 1_000_000) as usize); gpn();
    for s in 0..1024 {
        let pid = GD_PID[s].load(Relaxed);
        if pid == 0 { continue; }
        let mut v = [0u32; GD_K];
        for k in 0..GD_K { v[k] = GD_C[s][k].swap(0, Relaxed); }
        if v.iter().all(|&x| x < 20) { continue; }
        gp("[GDT] pid="); gd_dec(pid as usize);
        gp(" tgid="); gd_dec(GD_TGID[s].load(Relaxed) as usize);
        for k in 0..GD_K {
            if v[k] == 0 { continue; }
            gp(" "); gp(GD_NAMES[k]); gp("="); gd_dec(v[k] as usize);
        }
        gpn();
    }
    for i in 0..GD_FDS {
        let n = GD_FD_N[i].swap(0, Relaxed);
        if n < 10 { continue; }
        let key = GD_FD_KEY[i].load(Relaxed);
        gp("[GDFD] tgid="); gd_dec((key >> 32) as usize);
        gp(" fd="); gd_dec(((key >> 1) & 0xFFFF) as usize);
        gp(if key & 1 == 0 { " via=epoll" } else { " via=poll" });
        let kd = GD_FD_KIND[i].load(Relaxed);
        const KN: [&str; 12] = ["?", "pipe", "pty", "eventfd", "timerfd", "dev", "signalfd",
            "inotify", "file", "vfs", "socket", "epoll"];
        gp(" kind="); gp(KN[((kd >> 24) as usize).min(11)]);
        gp("/"); gd_dec((kd & 0xFF_FFFF) as usize);
        gp(" n="); gd_dec(n as usize); gpn();
    }
    for i in 0..GD_FDS { GD_FD_KEY[i].store(0, Relaxed); GD_FD_N[i].store(0, Relaxed); GD_FD_KIND[i].store(0, Relaxed); }
    for i in 0..GD_SCN {
        let n = GD_SCN_N[i].swap(0, Relaxed);
        if n < 50 { continue; }
        let key = GD_SCN_KEY[i].load(Relaxed);
        gp("[GDSC] pid="); gd_dec((key >> 16) as usize);
        gp(" nr="); gd_dec((key & 0xFFFF) as usize);
        gp(" n="); gd_dec(n as usize); gpn();
    }
    // Keys are claimed for good; start every window with empty tables so a
    // boot's worth of dead (pid, nr) / (tgid, fd) keys cannot fill them.
    for i in 0..GD_SCN { GD_SCN_KEY[i].store(0, Relaxed); GD_SCN_N[i].store(0, Relaxed); }
}

pub fn dispatch(
    number: usize,
    a0: usize, a1: usize, a2: usize,
    a3: usize, a4: usize, a5: usize,
    frame_ptr: usize,
) -> isize {
    dbg_serial_dump_maybe();
    sched::note_syscall_enter(number);
    let (sc_pid, sc_focus, sc_t0) = if SC_STATS {
        let pid = current_pid();
        LAST_SC[(pid as usize) & 1023].store(((pid as u64) << 32) | ((number as u64) << 1) | 1, Ordering::Relaxed);
        let f = sched::SC_FOCUS_TGID.load(Ordering::Relaxed);
        let f2 = sched::SC_FOCUS2_TGID.load(Ordering::Relaxed);
        let tg = sched::current_tgid();
        let focus = if f != 0 && tg == f { 1u8 } else if f2 != 0 && tg == f2 { 2 } else { 0 };
        (pid, focus, if focus != 0 { monotonic_ns() } else { 0 })
    } else { (0, 0u8, 0) };
    let ret = dispatch_inner(number, a0, a1, a2, a3, a4, a5, frame_ptr);
    if gd_stats() {
        gd_inc(K_SC);
        gd_scn(number);
        #[cfg(target_arch = "x86_64")]
        let rd = matches!(number, 0 | 19 | 45 | 47);
        #[cfg(target_arch = "aarch64")]
        let rd = matches!(number, 63 | 65 | 207 | 212);
        if ret == -11 && rd { gd_inc(K_EAGAIN); }
        #[cfg(target_arch = "x86_64")]
        let fx = number == 202;
        #[cfg(target_arch = "aarch64")]
        let fx = number == 98;
        if fx {
            match a1 & 0x7F {
                0 | 9 => gd_inc(match ret { 0 => K_FXW_OK, -110 => K_FXW_TMO, _ => K_FXW_AGAIN }),
                1 | 10 => {
                    gd_inc(K_FX_WAKE);
                    for _ in 0..(ret.max(0) as usize).min(64) { gd_inc(K_FX_WOKEN); }
                }
                _ => {}
            }
        }
    }
    if SC_STATS && sc_focus == 1 && SC_VMA_DUMP.load(Ordering::Relaxed) == 1 {
        SC_VMA_DUMP.store(2, Ordering::Relaxed);
        mm::gap2::s("[VMA] focus tgid dump follows\n");
        sched::dump_user_vma(0);
    }
    if SC_STATS {
        LAST_SC[(sc_pid as usize) & 1023].store(((sc_pid as u64) << 32) | ((number as u64) << 1), Ordering::Relaxed);
        if sc_focus != 0 && number < SC_SLOTS {
            let k = (sc_focus - 1) as usize;
            let dt = monotonic_ns().wrapping_sub(sc_t0);
            SC_N[k][number].fetch_add(1, Ordering::Relaxed);
            SC_NS[k][number].fetch_add(dt, Ordering::Relaxed);
            SC_MAX[k][number].fetch_max(dt, Ordering::Relaxed);
        }
    }
    sched::note_syscall_exit(ret);
    // The syscall is the deepest the kernel stack ever gets — every filesystem
    // and network server runs in kernel context off the back of one. Checking
    // here costs a mask and a compare, and it is the difference between an
    // overflow being named at the syscall that caused it and it being a page
    // fault somewhere else, minutes later, on a different task (TODO.md #15).
    if !sched::check_kernel_stack() {
        sched::report_stack_overflow(current_pid(), number);
        sched::exit_group(-1);
    }
    if ((SYSCALL_TRACE_EINVAL && ret == -22) || (SYSCALL_TRACE_EACCES && ret == -13)) && current_pid() >= 3 {
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
        crate::serial_print_str(if ret == -13 { "[SC-EACCES] nr=" } else { "[SC-EINVAL] nr=" });
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
    if SYSCALL_TRACE_EBADF && ret == -9 && current_pid() >= 3 {
        ebadf_trace(number, a0, a1, a2, frame_ptr);
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
    // Perform any VT switch requested from IRQ context (the Ctrl+Alt+Fn chord
    // recogniser runs on the input drain and can only store an atomic there),
    // and run the VT_PROCESS handshake watchdog. Two relaxed atomic loads when
    // there is nothing pending.
    tty_server::vt::poll_deferred();
    ret
}




/// Gate the per-mmap `[MMAP]` device-mapping debug prints. These fire on every
/// device mmap (e.g. DRM buffer maps) and flood the serial console during
/// GPU-heavy userspace (cosmic-comp DRM/GL init), drowning real output. Off by
/// default; flip to `true` when bringing up a device-mmap path.
const DBG_MMAP: bool = false;

/// Name every ENOMEM this file can return from the memory, address-space and
/// exec paths.
///
/// "Out of memory (os error 12)" from LeandrOS means one of about a dozen
/// unrelated things, only two of which are about RAM, and userspace sees the
/// errno and nothing else. Two investigations have now started from that errno
/// and gone looking for a leak that was not there. With this on, one boot says
/// which return fired.
///
/// Committed `false`: the sites are on hot paths and the console costs ~0.19 s
/// per newline. `ENOMEM_BUDGET` bounds a run that leaves it on.
///
/// `sys_mincore` is deliberately not instrumented. Its ENOMEM means "this range
/// is not mapped" and is a normal answer to a pointer probe, not a failure.
const DBG_ENOMEM: bool = false;

/// Line budget shared by every `enomem_site` call.
static ENOMEM_BUDGET: AtomicUsize = AtomicUsize::new(64);

/// Print `site` and return -12, so a call site reads `return enomem_site("...")`
/// and the errno cannot drift away from its explanation.
fn enomem_site(site: &str) -> isize {
    if DBG_ENOMEM
        && ENOMEM_BUDGET
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed,
                          |v| if v == 0 { None } else { Some(v - 1) })
            .is_ok()
    {
        crate::serial_print_str("[ENOMEM] ");
        crate::serial_print_str(site);
        crate::serial_print_str(" pid=");
        crate::serial_print_hex(current_pid() as usize);
        crate::serial_print_str("\n");
    }
    -12
}

/// As `enomem_site`, plus the reason `mm::vmm` recorded for the mapping call
/// that just failed -- a VMA overlap and an exhausted buddy order are the same
/// errno and completely different bugs.
fn enomem_map_site(site: &str) -> isize {
    if DBG_ENOMEM {
        crate::serial_print_str("[ENOMEM] reason=");
        crate::serial_print_str(mm::vmm::last_map_fail());
        crate::serial_print_str(" ");
    }
    enomem_site(site)
}

/// Log every syscall entry (number + pid) over serial. Extremely verbose —
/// enable only while bisecting a userland bring-up failure.
const SYSCALL_TRACE: bool = false;

/// Log every syscall that fails with EINVAL (nr + args + pid). Cheap and
/// high-signal while bringing up a new ported binary.
const SYSCALL_TRACE_EINVAL: bool = false;

/// Same for EACCES (nr + args + pid), for pinning down which syscall a
/// permission-enforcement failure comes from; shares the EINVAL printer.
const SYSCALL_TRACE_EACCES: bool = false;

/// Log the fd-lifecycle syscalls (pipe2/dup/dup2/dup3/fcntl/close/execve) with
/// their arguments AND return value, plus how sys_write resolved each fd.
///
/// This is the trace to enable when a shell redirection or pipeline misbehaves:
/// the failure mode is always "the fd the child ended up with is not the one the
/// parent installed", and that is invisible from source alone — the install can
/// be undone a step later by exec's close-on-exec sweep, by a dup that copied a
/// flag it should not have, or by a spawn path that never issued the dup at all.
///
/// Enable, reproduce (`/bin/seq 1 5 | /bin/wc -l`), and read the sequence:
/// expect pipe2 → fcntl(F_DUPFD_CLOEXEC)→N → clone → dup3(N,1,0) → execve, and
/// then `write fd=1 -> pipe`. A `write fd=1 -> console` line is the smoking gun.
const SYSCALL_TRACE_FDS: bool = false;

/// Log every syscall that fails with EBADF: nr, args, pid/tgid, the image name
/// and (aarch64) the user pc/lr plus a short frame-pointer backtrace. Capped
/// at `EBADF_TRACE_MAX` lines per boot.
const SYSCALL_TRACE_EBADF: bool = false;
const EBADF_TRACE_MAX: usize = 400;

fn ebadf_trace(number: usize, a0: usize, a1: usize, a2: usize, frame_ptr: usize) {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    if n >= EBADF_TRACE_MAX { return; }
    let tgid = sched::current_tgid();
    let mut name = [0u8; 128];
    let nl = sched::exe_basename(tgid, &mut name).unwrap_or(0).min(name.len());
    let base = &name[..nl];
    let _g = TRACE_LOCK.lock();
    crate::serial_print_str("[EBADF] n=");
    crate::serial_print_hex(n);
    crate::serial_print_str(" nr=");
    crate::serial_print_hex(number);
    crate::serial_print_str(" a0=");
    crate::serial_print_hex(a0);
    crate::serial_print_str(" a1=");
    crate::serial_print_hex(a1);
    crate::serial_print_str(" a2=");
    crate::serial_print_hex(a2);
    crate::serial_print_str(" pid=");
    crate::serial_print_hex(current_pid() as usize);
    crate::serial_print_str(" tgid=");
    crate::serial_print_hex(tgid as usize);
    crate::serial_print_str(" t=");
    crate::serial_print_hex(ticks() as usize);
    crate::serial_print_str(" ");
    crate::serial_print_str(core::str::from_utf8(base).unwrap_or("?"));
    #[cfg(target_arch = "aarch64")]
    if frame_ptr != 0 {
        let uf = unsafe { &*(frame_ptr as *const sched::context::UserFrame) };
        crate::serial_print_str(" pc=");
        crate::serial_print_hex(uf.elr_el1 as usize);
        crate::serial_print_str(" lr=");
        crate::serial_print_hex(uf.x[30] as usize);
        let mut fp = uf.x[29] as usize;
        for _ in 0..8 {
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
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = frame_ptr;
    crate::serial_print_str("\n");
}

/// Serializes multi-part trace prints — concurrent syscalls on other CPUs
/// otherwise interleave their serial output into an unreadable shuffle.
static TRACE_LOCK: spin::Mutex<()> = spin::Mutex::new(());

// ── Gated serial-dump diagnostic ──────────────────────────────────────────────
//
// Garble-proof guest-log capture. Under HVF, once a Wayland compositor
// (cosmic-comp) owns the console the PL011 RX FIFO starves: driver keystrokes
// drop, so `cat /tmp/foo` garbles and >/dev/console mangles argv. The PL011 TX,
// however, is always reliable (every kernel serial log uses it). This facility
// streams a fixed set of tmpfs guest files straight out the TX, framed with
// unique markers, entirely bypassing the console/tty and the driver's RX path.
//
// It runs from the top of `dispatch()` in normal syscall context (an arbitrary
// process's syscall) — NOT from IRQ/scheduler-lock context — so the tmpfs read
// (a plain spin-mutex) and the serial write are safe. A running session makes
// thousands of syscalls/sec, so the interval below is effectively precise. The
// whole frame is emitted under TRACE_LOCK so kernel trace/fault prints can't
// split it. Gated OFF by default; flip DBG_SERIAL_DUMP to true to periodically
// snapshot whole guest files. For pinpoint checkpoints prefer SYS_DBG_SERIAL_
// WRITE (below), which emits on demand with no periodic tmpfs polling or large
// serial bursts — cheaper and lower-risk under a churning session.
const DBG_SERIAL_DUMP: bool = false;

/// Gate SYS_DBG_SERIAL_WRITE: a userspace checkpoint syscall that writes a short
/// string straight to the serial TX (framed `[UCK] ...`). On-demand, bounded to
/// 256 bytes, no locks beyond TRACE_LOCK, no tmpfs polling. This is the
/// preferred garble-proof capture for instrumenting a silent userspace failure
/// (proven: it round-tripped cosmic-panel's layer-shell checkpoints under HVF
/// after cosmic-comp starved the console RX). Gated OFF by default; flip to true
/// and rebuild when instrumenting a userspace bring-up, then flip back.
const DBG_SERIAL_WRITE: bool = false;

/// Handle SYS_DBG_SERIAL_WRITE(ptr, len): copy up to 256 bytes of user text and
/// emit it framed on the serial TX. No-op (returns 0) unless DBG_SERIAL_WRITE.
/// Root only (EPERM otherwise, whether or not the gate is on): an
/// unprivileged process must not be able to inject lines into the kernel's
/// serial log, which test harnesses and boot checks parse.
/// SYS_SECPROBE(op, addr): check, from inside the kernel, that the AArch64
/// hardening is live. Root only (EPERM otherwise).
///   op 0: bit 0 = PAN in use, bit 1 = PAN strict.
///   op 1: a raw 8-byte load from user `addr`, outside `sched::uaccess`.
///         Returns 1 when PAN trapped it (audit mode lets it through
///         after the trap); in strict mode the caller is killed (SIGSEGV).
///         0 means the kernel read user memory unhindered.
///   op 2: call user `addr` as a function at EL1. PXN must refuse the
///         fetch, which kills the caller (SIGSEGV); a return means the
///         kernel ran user code, and its x0 is passed back.
///   op 3: raw user accesses PAN has trapped since boot (both modes).
///   op 4: a raw store to null-page `addr` (< 0x10000) with EPOLL_INSTANCES
///         held. Kills the caller (SIGSEGV); the lock must be released.
/// x86_64: the same ops with SMAP for PAN and SMEP for PXN.
fn sys_secprobe(op: usize, addr: usize) -> isize {
    if !sched::cred::current_is_root() { return -1; }
    {
        use core::sync::atomic::Ordering::SeqCst;
        use sched::uaccess as ua;
        match op {
            0 => {
                let m = ua::pan_mode();
                (if m != 0 { 1 } else { 0 }) | (if m == ua::PAN_STRICT { 2 } else { 0 })
            }
            1 => {
                if addr == 0 || !ua::user_range_ok(addr, 8) { return -14; }
                prefault_user_ro(addr, 8);
                ua::PAN_PROBE_HIT.store(false, SeqCst);
                ua::PAN_PROBE_ADDR.store(addr, SeqCst);
                let v = unsafe { core::ptr::read_volatile(addr as *const u64) };
                core::hint::black_box(v);
                ua::PAN_PROBE_ADDR.store(usize::MAX, SeqCst);
                ua::PAN_PROBE_HIT.load(SeqCst) as isize
            }
            2 => {
                if addr == 0 || !ua::user_range_ok(addr, 4) { return -14; }
                prefault_user_ro(addr, 4);
                // x86_64: SMEP's fault handler kills only for this address.
                ua::PAN_PROBE_ADDR.store(addr, SeqCst);
                let f: extern "C" fn() -> isize = unsafe { core::mem::transmute(addr) };
                let r = f();
                ua::PAN_PROBE_ADDR.store(usize::MAX, SeqCst);
                r
            }
            3 => ua::PAN_FAULTS.load(SeqCst) as isize,
            4 => {
                // A raw store to the (never mapped) null page with
                // EPOLL_INSTANCES held: the fault-kill path must release the
                // lock, or the exit path and every other epoll caller spin
                // on it forever (Pi 5 wedge, a9a61b2b).
                if addr >= 0x10000 { return -22; }
                let g = EPOLL_INSTANCES.lock();
                unsafe {
                    #[cfg(target_arch = "aarch64")]
                    core::arch::asm!("str xzr, [{0}]", in(reg) addr, options(nostack));
                    #[cfg(target_arch = "x86_64")]
                    core::arch::asm!("mov qword ptr [{0}], 0", in(reg) addr, options(nostack));
                }
                drop(g);
                -14
            }
            _ => -22,
        }
    }
}

fn sys_dbg_serial_write(ptr: usize, len: usize) -> isize {
    if !sched::cred::current_is_root() { return -1; }
    if !DBG_SERIAL_WRITE { return 0; }
    let n = len.min(256);
    if n == 0 || ptr == 0 { return 0; }
    let mut buf = [0u8; 256];
    let ok = with_current_address_space(|as_| as_.read_user_buf(ptr, &mut buf[..n]))
        .unwrap_or(false);
    if !ok { return -14; } // EFAULT
    let _g = TRACE_LOCK.lock();
    crate::serial_print_str("[UCK] ");
    crate::serial_write_raw(&buf[..n]);
    crate::serial_print_str("\n");
    n as isize
}

/// Files streamed each interval. Missing/empty/non-regular entries are skipped.
/// Small dedicated checkpoint files (panel.ckpt) beat verbose logs here: the
/// tmpfs per-file cap is 32 KiB, so a chatty RUST_LOG=debug can overflow before
/// the point of interest, whereas one-line checkpoint markers never do.
const DBG_SERIAL_DUMP_PATHS: &[&str] = &[
    "/tmp/panel.ckpt",
    "/tmp/panel.panic",
    "/tmp/comp.ckpt",
    "/tmp/panel.log",
];

/// Dump cadence in 100 Hz timer ticks (600 = 6 s).
const DBG_SERIAL_DUMP_INTERVAL_TICKS: u64 = 600;

static DBG_DUMP_LAST_TICK: AtomicU64 = AtomicU64::new(0);
static DBG_DUMP_BUSY: AtomicBool = AtomicBool::new(false);
/// Single 32 KiB staging buffer, exclusive via DBG_DUMP_BUSY (one dumper at a
/// time). Mutex avoids `static mut` unsafety; it is only ever taken by the sole
/// CPU that wins the busy CAS.
static DBG_DUMP_BUF: spin::Mutex<[u8; 32768]> = spin::Mutex::new([0u8; 32768]);

/// Emit the framed contents of the dump path list to the serial TX, at most
/// once per interval. Cheap early-out on the common (not-yet-due) path.
fn dbg_serial_dump_maybe() {
    if !DBG_SERIAL_DUMP { return; }
    let now = ticks();
    let last = DBG_DUMP_LAST_TICK.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < DBG_SERIAL_DUMP_INTERVAL_TICKS { return; }
    // Claim the interval; a losing racer just returns.
    if DBG_DUMP_BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    DBG_DUMP_LAST_TICK.store(now, Ordering::Relaxed);

    let mut buf = DBG_DUMP_BUF.lock();
    let _g = TRACE_LOCK.lock();
    for path in DBG_SERIAL_DUMP_PATHS {
        if let Some(n) = vfs::tmpfs_read_all(path, &mut buf[..]) {
            if n == 0 { continue; }
            crate::serial_print_str("\n===LDUMP<< ");
            crate::serial_print_str(path);
            crate::serial_print_str(" len=");
            crate::serial_print_hex(n);
            crate::serial_print_str(" tick=");
            crate::serial_print_hex(now as usize);
            crate::serial_print_str(" ===\n");
            crate::serial_write_raw(&buf[..n]);
            crate::serial_print_str("\n===LDUMP>> ");
            crate::serial_print_str(path);
            crate::serial_print_str(" ===\n");
        }
    }
    drop(_g);
    drop(buf);
    DBG_DUMP_BUSY.store(false, Ordering::Release);
}

/// One line of fd trace: `[FD] pid=<p> <what> a=<..> b=<..> c=<..> -> <ret>`.
fn trace_fd(what: &str, a: usize, b: usize, c: usize, ret: isize) {
    if !SYSCALL_TRACE_FDS { return; }
    let _g = TRACE_LOCK.lock();
    crate::serial_print_str("[FD] pid=");
    crate::serial_print_hex(current_pid() as usize);
    crate::serial_print_str(" ");
    crate::serial_print_str(what);
    crate::serial_print_str(" a=");
    crate::serial_print_hex(a);
    crate::serial_print_str(" b=");
    crate::serial_print_hex(b);
    crate::serial_print_str(" c=");
    crate::serial_print_hex(c);
    crate::serial_print_str(" -> ");
    crate::serial_print_hex(ret as usize);
    crate::serial_print_str("\n");
}

/// Records which route sys_write took for an fd — the single most diagnostic
/// fact when a pipeline's output lands in the wrong place.
fn trace_fd_route(fd: usize, route: &str) {
    if !SYSCALL_TRACE_FDS { return; }
    let _g = TRACE_LOCK.lock();
    crate::serial_print_str("[FD] pid=");
    crate::serial_print_hex(current_pid() as usize);
    crate::serial_print_str(" write fd=");
    crate::serial_print_hex(fd);
    crate::serial_print_str(" -> ");
    crate::serial_print_str(route);
    crate::serial_print_str("\n");
}

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
    let (a0, a2) = fd_handle_args(number, a0, a2);
    if let Some(e) = not_a_socket(number, a0) { return e; }
    match number {
        // ── Leandros-private IPC syscalls ───────────────────────────────────────
        SYS_IPC_SEND => sys_send(a0, a1, a2),
        SYS_IPC_RECV => sys_recv(a0, a1),
        SYS_IPC_CALL => sys_call(a0, a1, a2),
        SYS_PORT_CREATE => match port::create(current_pid()) {
            Some(p) => p as isize,
            None    => enomem_site("port_create/table-full"),
        },

        // ── Device enumeration (lsblk/lspci/lsusb) ──────────────────────────────
        SYS_BLKDEV_COUNT => drivers::blkdev::device_count() as isize,
        SYS_BLKDEV_INFO  => sys_blkdev_info(a0, a1),
        SYS_BLKDEV_NAME  => sys_blkdev_name(a0, a1),
        SYS_PCIDEV_COUNT => drivers::pci::scan().len() as isize,
        SYS_PCIDEV_INFO  => sys_pcidev_info(a0, a1),
        SYS_USBDEV_COUNT => drivers::usb_hcd::device_count() as isize,
        SYS_USBDEV_INFO  => sys_usbdev_info(a0, a1),
        SYS_DBG_SERIAL_WRITE => sys_dbg_serial_write(a0, a1),
        SYS_SECPROBE => sys_secprobe(a0, a1),
        SYS_MOUNTS_COUNT => vfs::list_mounts().iter().filter(|e| e.in_use).count() as isize,
        SYS_MOUNTS_INFO  => sys_mounts_info(a0, a1),

        // ── Memory ────────────────────────────────────────────────────────────
        MMAP     => sys_mmap(a0, a1, a2, a3, a4, a5),
        MUNMAP   => sys_unmap_mem(a0, a1),
        MPROTECT => sys_mprotect(a0, a1, a2),
        BRK      => sys_brk(a0),
        MREMAP   => sys_mremap(a0, a1, a2, a3, a4),
        MINCORE  => sys_mincore(a0, a1, a2),

        // ── Scheduling ────────────────────────────────────────────────────────
        SCHED_YIELD => { yield_now("syscall_yield"); 0 }

        // ── Process lifecycle ─────────────────────────────────────────────────
        // `exit` releases the fds itself: the process's with its last
        // thread, not with a leader that pthread_exits first (lane proclife).
        EXIT    => exit(a0 as i32),
        SYS_SPAWN => sys_spawn(a0, a1, a2),
        WAIT4   => sys_wait4(a0, a1, a2, a3),
        WAITID  => sys_waitid(a0, a1, a2, a3),
        // getpid(2) names the *process* (thread group), from every thread;
        // the thread's own id is gettid(2). This returned the calling
        // thread's id, so any non-main thread saw a different "pid" from
        // its own process: Firefox's IPC endpoints record getpid() when
        // created and MOZ_RELEASE_ASSERT it on bind from another thread
        // (`mMyProcInfo == EndpointProcInfo::Current()`), and died there.
        GETPID  => sched::pidns::self_v(sched::current_tgid()) as isize,
        GETPPID => sys_getppid(),

        // ── exec / fork ───────────────────────────────────────────────────────
        EXECVE  => {
            let res = sys_execve(a0, a1, a2);
            // ENOENT is a routine PATH-search probe (every Rust/libc PATH
            // lookup tries /usr/bin before /bin) — the caller reports it.
            // Linux prints nothing for a failed execve; only surface the
            // genuinely unusual failures (ENOEXEC, EFAULT, ...).
            if res < 0 && res != -2 {
                crate::serial_print_str("  [SYSCALL] sys_execve failed with error: ");
                crate::serial_print_hex(res as usize);
                crate::serial_print_str("\n");
            }
            res
        }
        CLONE   => sys_clone_or_fork(a0, a1, a2, a3, a4, frame_ptr),
        CLONE3  => sys_clone3(a0, a1, frame_ptr),
        #[cfg(not(target_arch = "aarch64"))]
        VFORK   => sys_vfork(frame_ptr),
        #[cfg(not(target_arch = "aarch64"))]
        FORK    => {
            // fd tables are keyed by tgid — see the identical note in
            // sys_clone_or_fork's fork arm.
            let parent_pid = sched::tgid_of(current_pid());
            // The child needs its own fd table; fail like Linux on a full
            // process pool rather than create a child whose fds are invisible.
            if !vfs::fd_table_slot_free() { return -11; } // EAGAIN
            // The fd table must be duplicated BEFORE the child is enqueued:
            // on SMP another CPU can run the child immediately, and its
            // first fd-allocating syscall would otherwise see an empty table.
            let mut child = 0u32;
            let ret = fork_current(frame_ptr, |child_pid| {
                child = child_pid;
                fork_dup_tables(parent_pid, child_pid);
            });
            if ret < 0 { undo_unborn_child(child); }
            ret
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
        PAUSE          => sys_pause(),

        // ── Threads ────────────────────────────────────────────────────────────
        SET_TID_ADDR => sys_set_tid_address(a0),
        FUTEX        => sys_futex(a0, a1, a2, a3, a4, a5),
        RESTART_SYSCALL => sys_restart_syscall(),
        SET_ROBUST_LIST => sched::robust::set(a0, a1),
        GET_ROBUST_LIST => sys_get_robust_list(a0, a1, a2),

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
        IOCTL       => sys_ioctl(a0, a1, a2, frame_ptr),
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
        PPOLL       => sys_ppoll(a0, a1, a2, a3, a4),
        #[cfg(not(target_arch = "aarch64"))]
        POLL        => sys_poll(a0, a1, a2 as isize),
        // Process management
        CHDIR       => sys_chdir(a0),
        FCHDIR      => sys_fchdir(a0),
        CHROOT      => sys_chroot(a0),
        GETCWD      => sys_getcwd(a0, a1),
        SETPGID     => sys_setpgid(a0, a1),
        GETPGID     => sys_getpgid(a0),
        SETSID      => { let r = sched::setsid(); if r > 0 { sched::pidns::self_v(r as u32) as isize } else { r } }
        GETSID      => match sched::pidns::arg(a0 as u32) { Ok(k) => { let r = sched::cred::sid_of(k); if r > 0 { sched::pidns::out(r as u32) as isize } else { r } } Err(e) => e },
        // GETPGRP is an alias for GETPGID(0) on x86-64 but shares the same
        // number as GETPGID on AArch64, so only emit this arm on x86-64.
        #[cfg(target_arch = "x86_64")]
        GETPGRP     => sched::pidns::out(sched::current_pgid()) as isize,
        SETUID => if !sched::ns::ids_mapped(0, &[a0 as u32]) { -22 } else if sched::set_current_uid(a0 as u32) { 0 } else { -1 }, // EPERM
        SETGID => if !sched::ns::ids_mapped(1, &[a0 as u32]) { -22 } else if sched::set_current_gid(a0 as u32) { 0 } else { -1 }, // EPERM
        SETRESUID => if !sched::ns::ids_mapped(0, &[a0 as u32, a1 as u32, a2 as u32]) { -22 } else if sched::set_current_resuid(a0 as u32, a1 as u32, a2 as u32) { 0 } else { -1 }, // EPERM
        SETRESGID => if !sched::ns::ids_mapped(1, &[a0 as u32, a1 as u32, a2 as u32]) { -22 } else if sched::set_current_resgid(a0 as u32, a1 as u32, a2 as u32) { 0 } else { -1 }, // EPERM
        SETGROUPS => sys_setgroups(a0, a1),
        // Both answer the previous filesystem id, changed or not (Linux).
        SETFSUID => sched::set_current_fsid(a0 as u32, false) as isize,
        SETFSGID => sched::set_current_fsid(a0 as u32, true) as isize,
        SETREUID  => if !sched::ns::ids_mapped(0, &[a0 as u32, a1 as u32]) { -22 } else { sched::cred::set_re_ids(a0 as u32, a1 as u32, false) },
        SETREGID  => if !sched::ns::ids_mapped(1, &[a0 as u32, a1 as u32]) { -22 } else { sched::cred::set_re_ids(a0 as u32, a1 as u32, true) },
        GETRESUID   => sys_getresxid(a0, a1, a2, false),
        GETRESGID   => sys_getresxid(a0, a1, a2, true),
        GETGROUPS   => sys_getgroups(a0, a1),
        UMASK       => sched::umask(a0 as u32) as isize,
        // Filesystem operations (writable for /tmp, read-only otherwise)
        MKDIRAT     => sys_mkdirat(a0, a1, a2),
        UNLINKAT    => sys_unlinkat(a0, a1, a2),
        RENAMEAT  => sys_renameat(a0, a1, a2, a3, 0),
        RENAMEAT2 => sys_renameat(a0, a1, a2, a3, a4),
        LINKAT      => sys_linkat(a0, a1, a2, a3, a4),
        SYMLINKAT   => sys_symlinkat(a0, a1, a2),
        FCHMOD      => sys_fchmod(a0, a1),
        // fchmodat(2) has no flags argument: x3/r10 is whatever the caller left
        // there, and reading it as AT_SYMLINK_NOFOLLOW turned a chmod into an
        // lchmod at random. fchmodat2 (Linux 6.6) is the one that takes flags.
        FCHMODAT    => sys_fchmodat(a0, a1, a2, 0),
        FCHMODAT2   => sys_fchmodat2(a0, a1, a2, a3),
        FCHOWN      => sys_fchown(a0, a1, a2),
        FCHOWNAT    => sys_fchownat(a0, a1, a2, a3, a4),
        TRUNCATE    => sys_truncate(a0, a1),
        FTRUNCATE   => sys_ftruncate(a0, a1),
        // Linux faccessat(2) has no flags argument: a3 is whatever the caller
        // left in the register and must be ignored (glibc emulates flags in
        // userspace only when faccessat2 is missing).
        FACCESSAT   => if a2 & !7 != 0 { -22 } else { sys_faccessat(a0, a1, a2, 0) },
        // faccessat2(dirfd, path, mode, flags) — same shape as faccessat plus a
        // real `flags` arg (AT_EACCESS/AT_SYMLINK_NOFOLLOW). musl/rustix `access`
        // probe faccessat2 first and only fall back to faccessat if it is NOT
        // ENOSYS; leaving it unimplemented surfaces an ENOSYS that some callers
        // propagate as a hard I/O error. Route it through the same handler.
        // Linux do_faccessat: unknown flag bits, or a mode outside rwx, are EINVAL.
        FACCESSAT2  => {
            const OK_FLAGS: usize = 0x200 /*AT_EACCESS*/ | 0x100 /*AT_SYMLINK_NOFOLLOW*/ | 0x1000 /*AT_EMPTY_PATH*/;
            if a3 & !OK_FLAGS != 0 || a2 & !7 != 0 { -22 } else { sys_faccessat(a0, a1, a2, a3) }
        }
        STATFS  => sys_statfs(a0, a1),
        FSTATFS => sys_fstatfs(a0, a1),
        FSYNC       => sys_fsync(a0, vfs::FSYNC_KIND_FILE),
        FDATASYNC   => sys_fsync(a0, vfs::FSYNC_KIND_DATA),
        SYNCFS      => sys_fsync(a0, vfs::FSYNC_KIND_SYNCFS),
        SYNC        => sys_sync(),
        REBOOT      => crate::power::sys_reboot(a0, a1, a2, a3),
        FALLOCATE   => sys_fallocate(a0, a1, a2, a3),
        UTIMENSAT   => sys_utimensat(a0, a1, a2, a3),
        #[cfg(not(target_arch = "aarch64"))]
        UTIMES      => sys_utimes(AT_FDCWD, a0, a1, false),
        #[cfg(not(target_arch = "aarch64"))]
        FUTIMESAT   => sys_utimes(a0, a1, a2, false),
        #[cfg(not(target_arch = "aarch64"))]
        UTIME       => sys_utimes(AT_FDCWD, a0, a1, true),
        MKNODAT     => sys_mknodat(a0, a1, a2, a3),
        #[cfg(not(target_arch = "aarch64"))]
        UNLINK => sys_unlinkat(AT_FDCWD, a0, 0),
        #[cfg(not(target_arch = "aarch64"))]
        MKDIR  => sys_mkdirat(AT_FDCWD, a0, a1),
        // x86-64's legacy rmdir(2) had no dispatch arm at all, so it fell
        // through to the unknown-syscall path. AT_REMOVEDIR (0x200) is what
        // makes sys_unlinkat pick VFS_RMDIR over VFS_UNLINK.
        #[cfg(not(target_arch = "aarch64"))]
        RMDIR  => sys_unlinkat(AT_FDCWD, a0, 0x200),
        #[cfg(not(target_arch = "aarch64"))]
        RENAME => sys_renameat(AT_FDCWD, a0, AT_FDCWD, a1, 0),
        #[cfg(not(target_arch = "aarch64"))]
        LINK    => sys_linkat(AT_FDCWD, a0, AT_FDCWD, a1, 0),
        #[cfg(not(target_arch = "aarch64"))]
        SYMLINK => sys_symlinkat(a0, AT_FDCWD, a1),
        // x86-64's legacy chmod(2)/chown(2)/lchown(2) used to be a blanket
        // -EROFS. AArch64 has no such syscall numbers at all — libc there
        // always emits fchmodat/fchownat, which have worked all along — so
        // this was a pure x86-64 regression: `chmod 600 /tmp/f` reported
        // "Read-only file system" on a writable tmpfs, and `install -m` failed
        // with it. They are exactly their *at forms with AT_FDCWD; lchown
        // additionally must not follow a final symlink (AT_SYMLINK_NOFOLLOW).
        #[cfg(not(target_arch = "aarch64"))]
        CHMOD  => sys_fchmodat(AT_FDCWD, a0, a1, 0),
        #[cfg(not(target_arch = "aarch64"))]
        CHOWN  => sys_fchownat(AT_FDCWD, a0, a1, a2, 0),
        #[cfg(not(target_arch = "aarch64"))]
        LCHOWN => sys_fchownat(AT_FDCWD, a0, a1, a2, AT_SYMLINK_NOFOLLOW),
        // creat(path, mode) == open(path, O_CREAT|O_WRONLY|O_TRUNC, mode).
        // The constant was declared but had no dispatch arm, so the call fell
        // through to the unknown-syscall path and reported ENOSYS.
        #[cfg(not(target_arch = "aarch64"))]
        CREAT  => sys_openat(AT_FDCWD, a0, 0o101 | 0o1000, a1),
        // AArch64 has no legacy mknod(2) — MKNOD and MKNODAT share syscall
        // number 33 there, so the MKNODAT arm above already covers it and a
        // second arm for the same value would be a duplicate match pattern.
        #[cfg(not(target_arch = "aarch64"))]
        MKNOD => sys_mknodat(AT_FDCWD, a0, a1, a2),
        #[cfg(not(target_arch = "aarch64"))]
        ACCESS      => sys_faccessat(AT_FDCWD, a0, a1, 0),

        MOUNT       => sys_mount(a0, a1, a2, a3, a4),
        UMOUNT2     => sys_umount2(a0, a1),
        PIVOT_ROOT  => sys_pivot_root(a0, a1),

        // ── Socket syscalls ───────────────────────────────────────────────────
        SOCKET      => sys_socket(a0, a1, a2),
        BIND        => sys_bind(a0, a1, a2),
        LISTEN      => sys_listen(a0, a1),
        ACCEPT      => sys_accept(a0, a1, a2, 0),
        ACCEPT4     => sys_accept(a0, a1, a2, a3),
        CONNECT     => sys_connect(a0, a1, a2),
        SENDTO      => sys_sendto(a0, a1, a2, a3, a4, a5),
        RECVFROM    => sys_recvfrom(a0, a1, a2, a3, a4, a5),
        SENDMSG     => sys_sendmsg(a0, a1, a2),
        RECVMSG     => sys_recvmsg(a0, a1, a2),
        SENDMMSG    => sys_sendmmsg(a0, a1, a2, a3),
        RECVMMSG    => sys_recvmmsg(a0, a1, a2, a3, a4),
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
        CLOCK_NANOSLEEP => sys_clock_nanosleep(a0, a1, a2, a3),
        TIMERFD_CREATE  => sys_timerfd_create(a0, a1),
        TIMERFD_SETTIME => sys_timerfd_settime(a0, a1, a2, a3),
        TIMERFD_GETTIME => sys_timerfd_gettime(a0, a1),
        GETTIMEOFDAY => sys_gettimeofday(a0, a1),
        SETTIMEOFDAY => sys_settimeofday(a0, a1),
        CLOCK_SETTIME => sys_clock_settime(a0, a1),
        SYSINFO      => sys_sysinfo(a0),
        SENDFILE     => sys_sendfile(a0, a1, a2, a3),
        // copy_file_range(fd_in, off_in, fd_out, off_out, len, flags) puts the
        // descriptors in a0/a2 with *offset pointers* in a1/a3, whereas
        // sys_sendfile takes (out_fd, in_fd, offset_ptr, count). This arm used
        // to forward the offset pointers as descriptors, so the copy silently
        // targeted nonexistent fds and returned success having moved nothing.
        // Offsets are passed as NULL, i.e. use the fds' own file positions.
        COPY_FILE_RANGE => sys_sendfile(a2, a0, 0, a4),
        MEMFD_CREATE => sys_memfd_create(a0, a1),
        // splice(2) moves file -> pipe -> stdout for uutils `cat`; see
        // sys_splice for why it returns short counts instead of looping.
        SPLICE       => sys_splice(a0, a1, a2, a3, a4, a5),
        TEE          => sys_tee(a0, a1, a2, a3),
        VMSPLICE     => sys_vmsplice(a0, a1, a2, a3),
        SETITIMER    => sys_setitimer(a0, a1, a2),
        GETITIMER    => sys_getitimer(a0, a1),
        SIGPENDING   => sys_sigpending(a0),
        #[cfg(not(target_arch = "aarch64"))]
        ALARM        => sys_alarm(a0),
        GETRANDOM    => sys_getrandom(a0, a1, a2),
        SECCOMP      => sys_seccomp(a0, a1, a2),
        PRCTL        => sys_prctl(a0, a1, a2, a3, a4),
        MADVISE      => sys_madvise(a0, a1, a2),
        MSYNC        => sys_msync(a0, a1, a2),
        MLOCK | MUNLOCK | MLOCKALL | MUNLOCKALL => 0,
        CLOCK_GETRES => sys_clock_getres(a0, a1),
        PREAD64      => sys_pread64(a0, a1, a2, a3),
        PWRITE64     => sys_pwrite64(a0, a1, a2, a3),
        PREADV       => sys_preadv2(a0, a1, a2, a3, 0, false),
        PWRITEV      => sys_pwritev2(a0, a1, a2, a3, 0, false),
        PREADV2      => sys_preadv2(a0, a1, a2, a3, a5, true),
        PWRITEV2     => sys_pwritev2(a0, a1, a2, a3, a5, true),
        TIMES        => sys_times(a0),
        #[cfg(not(target_arch = "aarch64"))]
        TIME         => sys_time(a0),

        // ── poll / select / epoll (Phase 9) ───────────────────────────────────
        SELECT   => sys_select(a0, a1, a2, a3, a4, false, 0),
        PSELECT6 => sys_select(a0, a1, a2, a3, a4, true, a5),
        EPOLL_CREATE1  => sys_epoll_create1(a0),
        EPOLL_CTL      => sys_epoll_ctl(a0, a1, a2, a3),
        EPOLL_PWAIT  => sys_epoll_pwait(a0, a1, a2, a3, a4, a5),
        EPOLL_PWAIT2 => sys_epoll_pwait2(a0, a1, a2, a3, a4, a5),
        EVENTFD2       => sys_eventfd2(a0, a1, frame_ptr),
        SIGNALFD4      => sys_signalfd4(a0, a1, a2, a3),

        // ── Scheduling policy/affinity ────────────────────────────────────────
        SCHED_SETSCHEDULER => sys_sched_setscheduler(a0, Some(a1), a2),
        SCHED_SETPARAM     => sys_sched_setscheduler(a0, None, a1),
        SCHED_GETSCHEDULER => 0, // SCHED_OTHER = 0
        SCHED_GETPARAM     => sys_sched_getparam(a0, a1),
        SCHED_SETAFFINITY  => sys_sched_setaffinity(a0, a1, a2),
        SCHED_GETAFFINITY  => sys_sched_getaffinity(a0, a1, a2),
        SCHED_GET_PRIORITY_MAX | SCHED_GET_PRIORITY_MIN => 0,
        SETPRIORITY        => sys_setpriority(a0, a1, a2),
        GETPRIORITY        => sys_getpriority(a0, a1),
        GETCPU             => sys_getcpu(a0, a1, a2),

        // ── Resource usage ────────────────────────────────────────────────────
        GETRUSAGE => sys_getrusage(a0, a1),

        // ── Capabilities ─────────────────────────────────────────────────────
        CAPGET => sys_capget(a0, a1),
        CAPSET => sys_capset(a0, a1),

        // ── Modern Linux (stubs) ──────────────────────────────────────────────
        MEMBARRIER  => 0,
        // NUMA memory policy on a single-node machine (node 0 only).
        GET_MEMPOLICY => sys_get_mempolicy(a0, a1, a2, a3, a4),
        SET_MEMPOLICY => sys_set_mempolicy(a0, a1, a2),
        MBIND         => sys_mbind(a0, a1, a2, a3, a4, a5),
        RSEQ        => sched::rseq::sys_rseq(a0, a1, a2, a3),
        STATX       => sys_statx(a0, a1, a2, a3, a4),
        OPENAT2     => sys_openat2(a0, a1, a2, a3),
        CLOSE_RANGE => sys_close_range(a0, a1, a2),
        UNSHARE => sys_unshare(a0),
        // tokio (brush's runtime) reaps children through these when
        // pidfd_open succeeds; see sys_pidfd_open.
        PIDFD_OPEN        => sys_pidfd_open(a0, a1),
        PIDFD_SEND_SIGNAL => sys_pidfd_send_signal(a0, a1, a2, a3),
        PIDFD_GETFD       => sys_pidfd_getfd(a0, a1, a2),

        // ── Extended attributes ───────────────────────────────────────────────
        SETXATTR    => sys_setxattr(a0, a1, a2, a3, a4, false, false),
        LSETXATTR   => sys_setxattr(a0, a1, a2, a3, a4, false, true),
        FSETXATTR   => sys_setxattr(a0, a1, a2, a3, a4, true, false),
        GETXATTR    => sys_getxattr(a0, a1, a2, a3, false, false),
        LGETXATTR   => sys_getxattr(a0, a1, a2, a3, false, true),
        FGETXATTR   => sys_getxattr(a0, a1, a2, a3, true, false),
        LISTXATTR   => sys_listxattr(a0, a1, a2, false, false),
        LLISTXATTR  => sys_listxattr(a0, a1, a2, false, true),
        FLISTXATTR  => sys_listxattr(a0, a1, a2, true, false),
        REMOVEXATTR  => sys_removexattr(a0, a1, false, false),
        LREMOVEXATTR => sys_removexattr(a0, a1, false, true),
        FREMOVEXATTR => sys_removexattr(a0, a1, true, false),

        // ── Credentials ───────────────────────────────────────────────────────
        GETUID  => sched::ns::view_id(0, sched::current_uid())  as isize,
        GETEUID => sched::ns::view_id(0, sched::current_euid()) as isize,
        GETGID  => sched::ns::view_id(1, sched::current_gid())  as isize,
        GETEGID => sched::ns::view_id(1, sched::current_egid()) as isize,
        GETTID    => sched::pidns::self_v(current_pid()) as isize,
        TGKILL    => sys_tgkill(a0, a1, a2),
        TKILL     => sys_tkill(a0, a1),
        RT_SIGQUEUEINFO   => sys_rt_sigqueueinfo(None, a0, a1, a2),
        RT_TGSIGQUEUEINFO => sys_rt_sigqueueinfo(Some(a0), a1, a2, a3),

        // ── Privilege ─────────────────────────────────────────────────────────
        // Root-only operations this kernel does not implement: an unprivileged
        // caller gets Linux's answer (EPERM — the capability check comes before
        // anything else there), root gets ENOSYS.
        ACCT | VHANGUP | SWAPON | SWAPOFF | KEXEC_LOAD => {
            if sched::current_euid() != 0 { -1 } else { log_enosys(number) }
        }
        INIT_MODULE   => crate::kmod::sys::sys_init_module(a0, a1, a2),
        FINIT_MODULE  => crate::kmod::sys::sys_finit_module(a0, a1, a2),
        DELETE_MODULE => crate::kmod::sys::sys_delete_module(a0, a1),
        KEXEC_FILE_LOAD => crate::kexec::sys_kexec_file_load(a0, a1, a2, a3, a4),
        #[cfg(not(target_arch = "aarch64"))]
        IOPL | IOPERM => if sched::current_euid() != 0 { -1 } else { log_enosys(number) },
        SETHOSTNAME   => sys_setxname(a0, a1, false),
        SETDOMAINNAME => sys_setxname(a0, a1, true),

        // ── Signal helpers ────────────────────────────────────────────────────
        SIGALTSTACK => sys_sigaltstack(a0, a1, frame_ptr),

        // ── Resource limits ───────────────────────────────────────────────────
        GETRLIMIT  => sys_getrlimit(a0, a1),
        SETRLIMIT  => sys_prlimit64(0, a0, a1, 0),

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
        // Linux exit_group(2) kills every thread in the calling process, not
        // just the caller. `sched::exit_group` owns that sequence (single
        // owner, siblings reaped before the address space can go, fd
        // teardown through the registered hook); this used to be an inline
        // copy of its loop, which did not take part in the owner protocol.
        EXIT_GROUP => sched::exit_group(a0 as i32),

        // ── File advise / range operations (advisory — safe to no-op) ────────
        POSIX_FADVISE | SYNC_FILE_RANGE | READAHEAD => 0,

        // ── inotify (valid fd, watches accepted, events never fire — K2) ───────
        INOTIFY_INIT1     => sys_inotify_init1(a0),
        INOTIFY_ADD_WATCH => sys_inotify_add_watch(a0, a1, a2),
        INOTIFY_RM_WATCH  => sys_inotify_rm_watch(a0, a1),

        _ => log_enosys(number),
    }
}

// ── IPC syscalls ──────────────────────────────────────────────────────────────

/// Credential check for a user send to `port`: 0, -EPERM, or -EBADF when the
/// port does not exist. A server (handler) port passes here and is then
/// deny-by-default: `port::user_send_allowed` admits only the messages its
/// user filter names (kernel-only ports: none); a queue port only admits
/// callers that `sched::cred::ipc_send_check` allows against its owner.
fn ipc_send_permitted(port: u32) -> isize {
    match port::send_target(port) {
        None => -9,
        Some((true, _)) if port::is_kernel_only(port) => -1, // EPERM
        Some((true, _)) => 0,
        Some((false, owner)) => sched::cred::ipc_send_check(owner),
    }
}

/// sys_send(port, msg_ptr, _msg_len) — copy message from caller and enqueue it.
///
/// Both the target and a caller-chosen `msg.reply_port` are checked: a server
/// handler delivers its answer to `reply_port`, so an unchecked one would let
/// a caller have the VFS server write a (forged-for-them) reply into another
/// user's reply queue. A reply port that does not exist is left alone — the
/// handler's delivery just fails, as before.
fn sys_send(port_id: usize, msg_ptr: usize, _msg_len: usize) -> isize {
    // Message must be naturally aligned (8-byte) so the read is defined.
    if !validate_user_ptr_aligned(msg_ptr, core::mem::size_of::<Message>(), 8) { return -14; }
    let msg: Message = match sched::uaccess::read_user::<Message>(msg_ptr) { Some(m) => m, None => return -14 };
    let r = ipc_send_permitted(port_id as u32);
    if r != 0 { return r; }
    if !port::user_send_allowed(port_id as u32, &msg) { return -1; } // EPERM
    if msg.reply_port != u32::MAX {
        let r = ipc_send_permitted(msg.reply_port);
        if r != 0 && r != -9 { return r; }
    }
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
                if !sched::uaccess::write_user::<Message>(msg_ptr, msg) { return -14; }
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
            match port::create_reply(caller) {
                Some(p) => { set_current_reply_port(p); p }
                None    => return enomem_site("sys_call/no-reply-port"),
            }
        }
    };

    // Read the message, stamp our reply port, and forward it to the server.
    let mut msg: Message = match sched::uaccess::read_user::<Message>(msg_ptr) { Some(m) => m, None => return -14 };
    let r = ipc_send_permitted(port_id as u32);
    if r != 0 { return r; }
    if !port::user_send_allowed(port_id as u32, &msg) { return -1; } // EPERM
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

/// Limit for the mmap paths that populate the whole range up front: device
/// apertures, MAP_SHARED tmpfs/memfd VMO frames and the eager private copy
/// (unaligned offset / non-f2fs file). Each costs memory proportional to `len`
/// at map time.
const MAP_MAX_BYTES: usize = 256 * 1024 * 1024; // 256 MiB
/// Demand-paged mappings — anonymous memory and private f2fs file mappings —
/// cost nothing until touched: the VMA is one record, page tables and frames
/// appear per faulted page, and the per-VMA frame table (`mm::pagevec`) is
/// chunked, so an untouched span allocates nothing and a touched one pays
/// 8 bytes per 2 MiB of span for the chunk directory. The limits only bound
/// virtual-address consumption: SpiderMonkey reserves ~2 GiB of JIT code
/// space and up to ~8 GiB per wasm memory with MAP_NORESERVE, and musl's
/// ld.so maps a library's whole span from its file before overlaying the
/// segments (scudo's .bss makes that ~512 MiB). The mmap allocator places
/// first-fit per address space in the 0x4000_0000..0x7f00_0000_0000 window
/// (~127 TiB, `mm::vmm::MMAP_BASE..MMAP_TOP`), reusing freed ranges, so
/// these sizes do not threaten the layout.
const ANON_MAP_MAX_BYTES: usize = 64 * 1024 * 1024 * 1024;
const FILE_LAZY_MAP_MAX_BYTES: usize = 64 * 1024 * 1024 * 1024;

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
///   - `addr != 0` (hint)  — use the hint if that range is free, else as below.
///   - `addr == 0`         — the lowest free range of the caller's address
///                           space (`AddressSpace::find_free_range`).
///
/// Returns the mapped virtual address on success, or a negative errno.
/// Report any mmap that takes longer than this, with where the time went.
/// Cheap (two clock reads per phase) and silent unless something is slow; it
/// exists because `[WDOG] ... cosmic-comp ... last syscall 9` was seen with no
/// way to say which mapping, of what, cost the seconds.
const MMAP_SLOW_NS: u64 = 500_000_000;
/// File mappings at least this long are reported as `[MMAP-BIG]`.
const MMAP_BIG_LEN: usize = 4 << 20;

/// Largest single VFS read the eager file-mmap copy issues before opening an
/// interrupt window (see the copy loop in `sys_mmap_inner`). 512 KiB is a few
/// ms of f2fs/virtio-blk on KVM and well under a tick-watchdog period on TCG.
const MMAP_READ_CHUNK: usize = 512 * 1024;

fn mmap_print_dec(mut n: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop { i -= 1; buf[i] = b"0"[0] + (n % 10) as u8; n /= 10; if n == 0 { break; } }
    crate::serial_print_str(core::str::from_utf8(&buf[i..]).unwrap_or("?"));
}

#[derive(Default)]
struct MmapTrace {
    /// 1 anon, 2 device, 3 shared-vmo, 4 file-copy
    kind: u8,
    map_ns: u64,
    io_ns: u64,
    fixup_ns: u64,
    bytes: usize,
}

fn sys_mmap(addr: usize, len: usize, prot: usize,
            flags: usize, fd: usize, off: usize) -> isize {
    let mut tr = MmapTrace::default();
    let t0 = monotonic_ns();
    // A file on a noexec mount cannot be mapped executable (Linux: EPERM),
    // private or shared. The VMA remembers it so mprotect refuses too.
    let noexec_file = flags & 0x20 == 0 && prot & 4 != 0
        && vfs::fd_on_noexec_mount(current_pid(), fd);
    if noexec_file { return -1; } // EPERM
    let mut r = sys_mmap_inner(addr, len, prot, flags, fd, off, &mut tr);
    if r > 0 && flags & 0x20 == 0 && fd != usize::MAX && len != 0
        && vfs::fd_on_noexec_mount(current_pid(), fd) {
        with_current_address_space_mut(|as_| as_.mark_noexec_mount(r as usize, len));
    }
    let _ = &mut r;
    let dt = monotonic_ns().wrapping_sub(t0);
    if CHURN_STATS { churn_note(0, tr.kind, len, (flags as u32) | ((prot as u32) << 24), dt, 0); }
    // Every big file mapping, however fast: the per-mmap latency of the
    // mappings that used to be copied eagerly (a Rust binary mapping its own
    // executable, ld-musl mapping libgallium).
    // Serial only: it is a per-mapping trace, not a console message (on a
    // real-hardware framebuffer console it scrolled the login prompt away).
    if (tr.kind == 4 || tr.kind == 5) && len >= MMAP_BIG_LEN {
        fn s(m: &str) { for &b in m.as_bytes() { unsafe { crate::arch_serial_putc(b) } } }
        fn d(mut n: usize) {
            let mut buf = [0u8; 20];
            let mut i = buf.len();
            loop { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; if n == 0 { break; } }
            for &b in &buf[i..] { unsafe { crate::arch_serial_putc(b) } }
        }
        s("[MMAP-BIG] pid=");
        d(current_pid() as usize);
        s(if tr.kind == 5 { " kind=file-lazy" } else { " kind=file-copy" });
        s(" len_kib=");
        d(len >> 10);
        s(" us=");
        d((dt / 1000) as usize);
        s("\n");
    }
    if dt >= MMAP_SLOW_NS {
        crate::serial_print_str("[MMAP-SLOW] pid=");
        mmap_print_dec(current_pid() as usize);
        crate::serial_print_str(" kind=");
        crate::serial_print_str(match tr.kind { 1 => "anon", 2 => "device", 3 => "shared-vmo", 4 => "file-copy", 5 => "file-lazy", _ => "?" });
        crate::serial_print_str(" len=");
        crate::serial_print_hex(len);
        crate::serial_print_str(" flags=");
        crate::serial_print_hex(flags);
        crate::serial_print_str(" fd=");
        mmap_print_dec(fd);
        crate::serial_print_str(" total_ms=");
        mmap_print_dec((dt / 1_000_000) as usize);
        crate::serial_print_str(" map_ms=");
        mmap_print_dec((tr.map_ns / 1_000_000) as usize);
        crate::serial_print_str(" io_ms=");
        mmap_print_dec((tr.io_ns / 1_000_000) as usize);
        crate::serial_print_str(" fixup_ms=");
        mmap_print_dec((tr.fixup_ns / 1_000_000) as usize);
        crate::serial_print_str(" bytes=");
        crate::serial_print_hex(tr.bytes);
        crate::serial_print_str("\n");
    }
    r
}

/// Anonymous `mmap`: `MAP_ANONYMOUS`, and any mapping of `/dev/zero`, which
/// Linux treats the same way (`MAP_PRIVATE` -> private anonymous memory,
/// `MAP_SHARED` -> a shared anonymous object that stays shared across fork;
/// see `mm::shmem`). `flags` carries Linux's MAP_SHARED (0x01) / MAP_FIXED
/// (0x10) bits.
fn mmap_anon(virt: usize, len: usize, place: MmapCand, flags: usize,
             page_flags: PageFlags, prot: usize, tr: &mut MmapTrace) -> isize {
    const MAP_SHARED: usize = 0x01;
    const MAP_FIXED:  usize = 0x10;
    tr.kind = 1;
    let is_shared = flags & MAP_SHARED != 0;
    let tm = monotonic_ns();
    // L1.13: a big MAP_FIXED destination goes in bounded holds first; the in-hold
    // unmap below then only sees empty VMAs.
    if flags & MAP_FIXED != 0 && len > UNMAP_CHUNK_PAGES * mm::buddy::PAGE_SIZE { unmap_chunked(virt, len); }
    let mapped = with_current_address_space_mut(|as_| {
        let at = if flags & MAP_FIXED != 0 {
            as_.unmap_range(virt, len);
            virt
        } else {
            match mmap_place(as_, virt, len, place) { Some(a) => a, None => return None }
        };
        // Record the real protection: PROT_NONE (thread-stack guards,
        // reservations) must fault, not read as zero (see the fault path).
        Some((as_.map_lazy(at, len, page_flags, is_shared) && { as_.set_prot(at, prot as u32 & 7); true }, at))
    });
    tr.map_ns = monotonic_ns().wrapping_sub(tm);
    match mapped {
        Some(Some((true, at))) => at as isize,
        Some(_) => enomem_map_site("mmap/anon"),
        None => -1,
    }
}

/// Count of DRM-node mmap refusals (the first few are logged).
static MMAP_DRM_REFUSED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn sys_mmap_inner(addr: usize, len: usize, prot: usize,
            flags: usize, fd: usize, off: usize, tr: &mut MmapTrace) -> isize {
    // Linux mmap flags.
    const MAP_SHARED:    usize = 0x01;
    const MAP_FIXED:     usize = 0x10;
    const MAP_ANONYMOUS: usize = 0x20;
    const MAP_FIXED_NOREPLACE: usize = 0x10_0000;
    // Slot-1 mapping hint a device server may set in its 0x1007 reply: the
    // range it just handed back must not be mapped write-back. Mirrored in
    // servers/drm/src/lib.rs — the two must agree.
    const MMAP_HINT_UNCACHED: u64 = 1;

    if len == 0 { return -22; } // EINVAL

    let page = mm::buddy::PAGE_SIZE;
    let len  = (len + page - 1) & !(page - 1);
    let max_bytes = if flags & MAP_ANONYMOUS != 0 {
        ANON_MAP_MAX_BYTES
    } else {
        FILE_LAZY_MAP_MAX_BYTES
    };
    if len > max_bytes { return -22; }
    // RLIMIT_AS, and RLIMIT_DATA for a private writable mapping.
    {
        const PROT_WRITE: usize = 2;
        let data = flags & MAP_SHARED == 0 && prot & PROT_WRITE != 0;
        if !vm_limits_admit(len, data) { return -12; } // ENOMEM
    }
    // Every file path except the demand-paged private f2fs one populates the
    // range at map time; those keep the old ceiling (checked on entry below).
    let eager_too_big = len > MAP_MAX_BYTES;

    let page_flags = prot_to_page_flags(prot);

    // W^X enforcement.
    if page_flags.contains(PageFlags::WRITABLE) && page_flags.contains(PageFlags::EXECUTE) {
        return -22;
    }

    // Determine the virtual address to use.
    // A hint is only a hint: one that is unaligned or does not fit below
    // USER_SPACE_END is ignored, as on Linux, rather than failing the call.
    // SpiderMonkey and mozjemalloc probe random addresses across a 48-bit
    // space and only fall back to an unhinted mapping when a hinted one comes
    // back somewhere else.
    //
    // Every non-fixed path places through `mmap_place`, under the same
    // address-space hold as the map itself (so two threads cannot pick the
    // same hole): a hint is taken if that range is free, and otherwise the
    // lowest free range of this address space is used, so freed ranges are
    // reused. MAP_FIXED replaces whatever is there.
    let hint_usable = addr != 0 && addr & (page - 1) == 0
        && addr.checked_add(len).map_or(false, |e| e <= USER_SPACE_END);
    // MAP_FIXED_NOREPLACE (Linux 4.17+): exactly at `addr`, EEXIST if anything
    // is there, never unmapping. It wins over MAP_FIXED, as on Linux, so it is
    // carried below as a non-fixed mapping placed `MmapCand::Exact`.
    let noreplace = flags & MAP_FIXED_NOREPLACE != 0;
    let flags = if noreplace { flags & !MAP_FIXED } else { flags };
    let (virt, place) = if noreplace {
        if !hint_usable { return -22; }
        (addr, MmapCand::Exact)
    } else if flags & MAP_FIXED != 0 {
        if addr == 0 { return -22; }
        (addr, MmapCand::Exact)
    } else if hint_usable {
        (addr, MmapCand::Hint)
    } else {
        (0, MmapCand::Any)
    };
    if noreplace
        && with_current_address_space(|as_| as_.overlap_end(virt, len).is_some()) == Some(true)
    {
        return -17; // EEXIST
    }

    let end = match virt.checked_add(len) {
        Some(e) => e,
        None    => return -22,
    };
    if end > USER_SPACE_END { return -22; }

    // ── Anonymous mmap ────────────────────────────────────────────────────────
    if flags & MAP_ANONYMOUS != 0 {
        return mmap_anon(virt, len, place, flags & (MAP_SHARED | MAP_FIXED), page_flags, prot, tr);
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
    let tk = monotonic_ns();
    let kind = vfs::vfs_get_node_kind(pid, fd);
    tr.io_ns = monotonic_ns().wrapping_sub(tk);
    // /dev/zero maps as anonymous memory (the offset is ignored), as on
    // Linux. It used to take the eager private-copy path below: a MAP_SHARED
    // /dev/zero region was neither shared across fork nor demand-paged.
    if let Some(vfs::VnodeKind::DevZero) = kind {
        return mmap_anon(virt, len, place, flags & (MAP_SHARED | MAP_FIXED), page_flags, prot, tr);
    }
    let mut phys_addr: usize = 0;
    // Set from slot 1 of the device's 0x1007 reply; see MMAP_HINT_UNCACHED.
    let mut dev_uncached = false;
    let mut v4l2_pins: Option<Vec<usize>> = None;
    // /dev/video0: the V4L2 layer hands out its frame buffers directly.
    if let Some(vfs::VnodeKind::DynamicDevice { port, dev_id, open_id }) = kind {
        if vfs::dynamic_device_major(port, dev_id) == Some(v4l2::VIDEO_MAJOR) {
            match v4l2::mmap_frames(open_id, off, len) {
                Ok(f) => { phys_addr = f[0]; v4l2_pins = Some(f); }
                Err(e) => return e,
            }
        }
    }

    if let Some(vfs::VnodeKind::DynamicDevice { port, dev_id, open_id }) = kind.filter(|_| phys_addr == 0) {
        if DBG_MMAP {
        crate::serial_print_str("[MMAP] DynamicDevice fd, off=");
        crate::serial_print_hex(off);
        crate::serial_print_str("\n");
        }
        // This is a dynamic device, call its ioctl to get the physical address
        let mut proxy_msg = Message::empty();
        proxy_msg.tag = vfs::VFS_IOCTL as u64;
        proxy_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
        proxy_msg.data[8..16].copy_from_slice(&(0x1007u64).to_le_bytes()); // DRM_IOCTL_MMAP
        proxy_msg.data[16..24].copy_from_slice(&(off as u64).to_le_bytes()); // Pass requested physical address
        proxy_msg.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes()); // PID
        // Slot 4: the per-open identity, same as the VFS ioctl proxy sends, so
        // a device server sees one consistent open across ioctl and mmap.
        proxy_msg.data[32..40].copy_from_slice(&(open_id as u64).to_le_bytes());

        tr.kind = 2;
        let tc = monotonic_ns();
        let reply = vfs::call_port(port, proxy_msg);
        tr.io_ns += monotonic_ns().wrapping_sub(tc);
        if reply.tag == 0 {
            let res = u64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8])) as usize;
            if DBG_MMAP {
            crate::serial_print_str("[MMAP] device ioctl returned phys=");
            crate::serial_print_hex(res);
            crate::serial_print_str("\n");
            }
            // Device servers answer every request with tag 0 and put a
            // negative errno in slot 0 on failure (servers/drm make_reply).
            // That value used to be taken as a physical address: a refused
            // token mapped phys 0xFFFF_FFFF_FFFF_F000 and then, wrapping, the
            // bottom of physical memory -- reads mostly zero, stores into low
            // RAM. An errno is a refusal, never a frame.
            let refused = (res as isize) < 0 && (res as isize) > -4096;
            if res != 0 && !refused {
                phys_addr = res;
                // Slot 1 is the device server's mapping hint. MMAP_HINT_UNCACHED
                // means "this physical range is not cache-coherent with whoever
                // else writes it — do not map it write-back". Only the DRM
                // server sets it today, and only for a virtio-gpu host-visible
                // blob the host asked to be WC or UNCACHED (see
                // `blob_map_cache_type` in drivers/src/drm_device_interface.rs);
                // every other device leaves the slot zero and is mapped exactly
                // as before.
                let hint = u64::from_le_bytes(
                    reply.data[8..16].try_into().unwrap_or([0u8; 8]));
                dev_uncached = hint == MMAP_HINT_UNCACHED;
            }
        } else if DBG_MMAP {
            crate::serial_print_str("[MMAP] device ioctl reply tag non-zero\n");
        }
        // A DRM node that refuses the token (one it never handed out, or one
        // whose object is gone) must FAIL the mmap. Falling through to the
        // file-backed path below "succeeded" with a private copy of the
        // node's read() -- zero bytes -- i.e. a zero-filled anonymous mapping
        // whose stores go nowhere: exactly iris reading zeros back from a
        // buffer the GPU had rendered while its CPU writes vanished (laptop
        // boot 15b comp-lin-first). Other device servers keep the fallthrough.
        if phys_addr == 0 && vfs::dynamic_device_major(port, dev_id) == Some(226) {
            // (offset 0 with no framebuffer lands here too: same answer)
            if MMAP_DRM_REFUSED.fetch_add(1, Ordering::Relaxed) < 32 {
                crate::serial_print_str("[MMAP] DRM node refused mmap token ");
                crate::serial_print_hex(off);
                crate::serial_print_str(" len ");
                crate::serial_print_hex(len);
                crate::serial_print_str(" pid ");
                crate::serial_print_hex(pid as usize);
                crate::serial_print_str("\n");
            }
            return -22; // EINVAL
        }
    }

    if phys_addr != 0 {
        // This is a device mapping — map the physical address directly.
        if eager_too_big { return -22; }
        //
        // WRITECOMBINE rather than NOCACHE: on AArch64 NOCACHE means Device
        // memory (MAIR index 3), which forbids unaligned access and is wrong for
        // a buffer userspace memcpys through. WRITECOMBINE is Normal
        // Non-cacheable there and PCD (UC) on x86-64 — non-cached on both, and
        // still ordinary memory on both.
        let dev_flags = if dev_uncached {
            page_flags | PageFlags::WRITECOMBINE
        } else {
            page_flags
        };
        let tm = monotonic_ns();
        // L1.14: a mapping of a guest-RAM BO (dumb, guest blob, v3d, or a
        // scattered virgl buffer, L1.10) is a reference on its pages: every
        // frame is pinned (+1) under the BO map's lock and installed with
        // `map_shared_frames`, whose VMA owns the pins, so GEM_CLOSE or the
        // open going away cannot return the frames to the allocator while they
        // are mapped (munmap -- also partial --, fork and exit release them
        // page by page). Tokens that name no guest-RAM BO (firmware
        // framebuffer, host-visible window, GPU apertures) keep the plain
        // device mapping. Asked before the AS hold (the BO maps are never
        // taken under it).
        let pinned = v4l2_pins.take().or_else(|| match kind {
            Some(vfs::VnodeKind::DynamicDevice { open_id, .. }) =>
                drivers::drm_device_interface::mmap_pin_frames(phys_addr, len, open_id as u32),
            _ => None,
        });
        if pinned.as_ref().map_or(false, |f| f.is_empty()) { return -22; }
        // L1.14, host-visible window (no guest RAM to pin): the window span is
        // held by its map count until this VMA goes (munmap / exit release it
        // through the mm device-VMA hook), so a dead blob's offset cannot be
        // handed to a new blob under a live mapping. A token in the window that
        // names no live span, or a length running past it, is refused.
        let win_pin = drivers::drm_device_interface::mmap_pin_window(phys_addr, len);
        if win_pin == Some(false) { return -22; } // (window tokens are never RAM: `pinned` is None)
        // openbugs #94/#95: a device token that pinned nothing and is no window
        // span is mapped unreferenced -- only acceptable for MMIO / BAR /
        // firmware framebuffer. RAM here means the buffer the ioctl vouched for
        // was freed (GEM_CLOSE on another thread) before the pin, or a device
        // server handed out RAM with no pin, which device close / stream stop /
        // realloc would free under the mapping. Refused for every device, not
        // only DRM nodes (drivers/src/device_mmap_policy.rs).
        {
            use drivers::device_mmap_policy::{decide, Decision, Pin, Window};
            let p = match &pinned { None => Pin::NotGuestRam, Some(f) if f.is_empty() => Pin::Refused, Some(_) => Pin::Pinned };
            let w = match win_pin { None => Window::NotWindow, Some(false) => Window::Refused, Some(true) => Window::Held };
            let ram = p == Pin::NotGuestRam && w == Window::NotWindow && mm::buddy::is_ram(phys_addr, len);
            if decide(p, w, ram) == Decision::Refuse {
                if MMAP_DRM_REFUSED.fetch_add(1, Ordering::Relaxed) < 32 {
                    crate::serial_print_str("[MMAP] device token names RAM but no live pinned buffer ");
                    crate::serial_print_hex(phys_addr);
                    crate::serial_print_str("\n");
                }
                return -22;
            }
        }
        let release_pins = |f: &alloc::vec::Vec<usize>| {
            for &p in f.iter() { mm::pageref::unref_or_free(p, 0); }
        };
        // L1.13: a big MAP_FIXED destination goes in bounded holds first; the in-hold
        // unmap below then only sees empty VMAs.
        if flags & MAP_FIXED != 0 && len > UNMAP_CHUNK_PAGES * mm::buddy::PAGE_SIZE { unmap_chunked(virt, len); }
        let mut pins_consumed = false;
        let mapped = with_current_address_space_mut(|as_| {
            let at = if flags & MAP_FIXED != 0 {
                as_.unmap_range(virt, len);
                virt
            } else {
                match mmap_place(as_, virt, len, place) { Some(a) => a, None => return None }
            };
            let ok = match &pinned {
                None => as_.map_device(at, phys_addr, len, dev_flags),
                Some(frames) => {
                    // Consumes the pins on success and on failure alike.
                    pins_consumed = true;
                    as_.map_shared_frames(at, frames, dev_flags)
                }
            };
            Some((ok, at))
        });
        if let Some(f) = &pinned { if !pins_consumed { release_pins(f); } }
        tr.map_ns = monotonic_ns().wrapping_sub(tm);
        let ret = match mapped {
            Some(Some((true, at))) => at as isize,
            _ => {
                if win_pin == Some(true) {
                    drivers::drm_device_interface::hostvis_vma_release(phys_addr, len);
                }
                enomem_map_site("mmap/device")
            }
        };
        if DBG_MMAP {
        crate::serial_print_str("[MMAP] map_device virt=");
        crate::serial_print_hex(ret as usize);
        crate::serial_print_str(" result=");
        crate::serial_print_hex(ret as usize);
        crate::serial_print_str("\n");
        }
        return ret;
    }

    // ── Shared tmpfs/memfd mmap (K1 shared VMO) ───────────────────────────────
    // MAP_SHARED of a tmpfs/memfd file aliases the file's VMO frames rather
    // than copying them, so two mappings — or a mapping and read()/write() — of
    // the same inode see each other's stores. The frames are pinned by the VFS
    // acquire (pageref::inc each, under the VMO lock) BEFORE we take AS-busy to
    // map them (lock order: never nest the tmpfs locks under AS-busy). f2fs
    // MAP_SHARED still falls through to the eager private-copy path below.
    //
    // A MAP_PRIVATE of a tmpfs file above the eager-copy ceiling takes this
    // path too (git maps a whole packfile that way; it was EINVAL above
    // 256 MiB). Aliasing the VMO frames is Linux's own behaviour for unmodified
    // private pages, and it needs no contiguous copy block. The VMA is a CoW
    // region (map_private_vmo_frames): a store, also after mprotect(PROT_WRITE)
    // or from a PROT_WRITE map, copies the page and never reaches the file.
    let tmp_private_big_ro = flags & MAP_SHARED == 0 && eager_too_big
        && off % mm::buddy::PAGE_SIZE == 0 && len <= ANON_MAP_MAX_BYTES;
    if flags & MAP_SHARED != 0 || tmp_private_big_ro {
        if let Some(vfs::VnodeKind::TmpFile { idx, .. }) = kind {
            // MAP_SHARED aliases the VMO frames with no copy, so the eager
            // ceiling does not apply (only the address-space cap does).
            if len > ANON_MAP_MAX_BYTES { return -22; }
            tr.kind = 3;
            // Lazy MAP_SHARED (lane mmapB): the VMA only names the object;
            // pages are found on first touch outside the tmpfs locks, so a
            // 4 MiB map is O(1) here instead of 1024 allocs + zeroing + PTEs.
            // Dmabuf/borrowed objects and ranges past EOF keep the eager path.
            if flags & MAP_SHARED != 0 {
                if let Some(vmo_id) = vfs::vmo_lazy_prepare(pid, fd, off, len) {
                    if flags & MAP_FIXED != 0 && len > UNMAP_CHUNK_PAGES * mm::buddy::PAGE_SIZE { unmap_chunked(virt, len); }
                    let placed = with_current_address_space_mut(|as_| {
                        let at = if flags & MAP_FIXED != 0 {
                            as_.unmap_range(virt, len);
                            virt
                        } else {
                            match mmap_place(as_, virt, len, place) { Some(a) => a, None => return None }
                        };
                        Some((as_.map_shared_vmo_lazy(at, len, page_flags, vmo_id, off as u64), at))
                    });
                    return match placed {
                        Some(Some((true, at))) => at as isize,
                        Some(Some((false, _))) => enomem_map_site("mmap/shared-vmo-lazy"),
                        _ => enomem_site("mmap/shared-vmo-lazy/no-room"),
                    };
                }
            }
            let ta = monotonic_ns();
            let acquired = vfs::vmo_acquire_frames(pid, fd, off, len);
            tr.io_ns += monotonic_ns().wrapping_sub(ta);
            match acquired {
                Some((frames, vmo_id)) => {
                    // i915.ptecheck: a dma-buf mmap must alias the object's own pages.
                    if let Some(obj) = vfs::dmabuf_obj_of(sched::tgid_of(pid), fd) {
                        drivers::i915::ptecheck_dmabuf_mmap(obj, off as u64, &frames);
                    }
                    // [GAP2] snapshot before the frames are consumed by the map.
                    let g2_p0 = frames.first().copied().unwrap_or(0);
                    let g2_n  = frames.len();
                    // L1.13: a big MAP_FIXED destination goes in bounded holds first; the in-hold
                    // unmap below then only sees empty VMAs.
                    if flags & MAP_FIXED != 0 && len > UNMAP_CHUNK_PAGES * mm::buddy::PAGE_SIZE { unmap_chunked(virt, len); }
                    let placed = with_current_address_space_mut(|as_| {
                        let at = if flags & MAP_FIXED != 0 {
                            as_.unmap_range(virt, len);
                            virt
                        } else {
                            match mmap_place(as_, virt, len, place) { Some(a) => a, None => return None }
                        };
                        let ok = if flags & MAP_SHARED == 0 {
                            as_.map_private_vmo_frames(at, &frames, page_flags)
                        } else {
                            as_.map_shared_frames(at, &frames, page_flags)
                        };
                        // Named by its object: truncation, mremap growth and
                        // /proc/<pid>/maps find it through this.
                        if ok { as_.set_vmo(at, vmo_id, off as u64); }
                        Some((ok, at))
                    });
                    // No room: nothing consumed the pins (same as the no-AS case).
                    let (mapped, virt) = match placed {
                        Some(Some((ok, at))) => (Some(ok), at),
                        Some(None) | None => (None, virt),
                    };
                    // Logged OUTSIDE the closure: inside it the address space
                    // `busy` flag is held.
                    if mm::gap2::budget_ok() {
                        mm::gap2::s("[G2MAP] pid="); mm::gap2::h(pid as usize);
                        mm::gap2::kv(" fd=",   fd);
                        mm::gap2::kv(" idx=",  idx);
                        mm::gap2::kv(" off=",  off);
                        mm::gap2::kv(" len=",  len);
                        mm::gap2::kv(" prot=", prot);
                        mm::gap2::kv(" mflg=", flags);
                        mm::gap2::kv(" virt=", virt);
                        mm::gap2::kv(" p0=",   g2_p0);
                        mm::gap2::kv(" n=",    g2_n);
                        mm::gap2::kv(" rc=",   match mapped {
                            Some(true) => 1, Some(false) => 0, None => 2 });
                        mm::gap2::nl();
                    }
                    return match mapped {
                        // On success the VMA owns the transferred pins.
                        Some(true)  => virt as isize,
                        // map_shared_frames released the pins itself on failure.
                        Some(false) => enomem_map_site(("mmap/shared-vmo")),
                        // No current address space: release the pins ourselves.
                        None => { vfs::vmo_release_frames(&frames); enomem_site("mmap/shared-vmo/no-address-space") }
                    };
                }
                None => return enomem_site("mmap/shared-vmo/acquire-frames")
            }
        }
    }

    // ── Private f2fs file mmap: demand-paged ──────────────────────────────────
    // No read here at all: the VMA names the file's inode and each page is
    // read on first touch (see map_private_file / install_file_fault), with
    // Linux EOF semantics. mmap(2) never moves the descriptor's position.
    // Unaligned offsets (EINVAL on Linux) keep the old eager copy below.
    //
    // A READ-ONLY MAP_SHARED mapping takes this path too (lane nvstall). It
    // used to fall through to the eager copy: the whole file read at mmap
    // time into private frames of this process. fontdb (cosmic-text) maps
    // every font in the system that way in every COSMIC process — a dozen
    // panel applets times every font file — and on the PC's USB stick that
    // was the four-minute I/O storm behind boot6's `[MMAP-SLOW] ... flags=1`
    // lines and tick stalls. Demand paging reads only the pages touched
    // (fontdb parses a few table headers per face) and shares them through
    // the page cache. The eager copy was a private snapshot as well, so no
    // coherence is lost; a writable MAP_SHARED mapping still copies eagerly.
    //
    // MAP_SHARED from a descriptor open read-write (lane pagecache): every
    // page is the file's page-cache frame itself, so a store through the
    // mapping is what read(2) and every other mapping see, and reaches the
    // file through write-back (msync, fsync, munmap + the background task).
    // A writable MAP_SHARED needs O_RDWR (EACCES otherwise, as on Linux);
    // a read-only one from an O_RDONLY descriptor stays a private mapping —
    // it can never write the file, whatever mprotect later asks. With
    // `pcache=0` a writable MAP_SHARED is the old eager private copy.
    const PROT_WRITE: usize = 0x2;
    const O_ACCMODE: u32 = 3;
    const O_RDWR: u32 = 2;
    let mounted = matches!(kind, Some(vfs::VnodeKind::MountedFile { .. }));
    let acc_rdwr = mounted && vfs::fd_flags(pid, fd).map_or(false, |f| f & O_ACCMODE == O_RDWR);
    if mounted && flags & MAP_SHARED != 0 && prot & PROT_WRITE != 0 && !acc_rdwr {
        return -13; // EACCES
    }
    let shared_cached = flags & MAP_SHARED != 0 && acc_rdwr && mm::pagecache::read_cache_enabled();
    if (flags & MAP_SHARED == 0 || prot & PROT_WRITE == 0 || shared_cached) && off % page == 0 {
        if let Some(vfs::VnodeKind::MountedFile { port, file_id }) = kind {
            if let Some(cap) = f2fs_server::inode_by_port(port, file_id as u64)
                .and_then(|ino| mmap_file_register(port, ino))
            {
                tr.kind = 5;
                let tm = monotonic_ns();
                let map = |as_: &mut mm::vmm::AddressSpace, at: usize| {
                    if shared_cached { as_.map_shared_file(at, len, page_flags, cap, off as u64) }
                    else { as_.map_private_file(at, len, page_flags, cap, off as u64) }
                };
                // L1.13: a big MAP_FIXED destination goes in bounded holds first; the in-hold
                // unmap below then only sees empty VMAs.
                if flags & MAP_FIXED != 0 && len > UNMAP_CHUNK_PAGES * mm::buddy::PAGE_SIZE { unmap_chunked(virt, len); }
                let mapped = with_current_address_space_mut(|as_| {
                    let at = if flags & MAP_FIXED != 0 {
                        as_.unmap_range(virt, len);
                        virt
                    } else {
                        match mmap_place(as_, virt, len, place) { Some(a) => a, None => return None }
                    };
                    Some((map(as_, at), at))
                });
                tr.map_ns = monotonic_ns().wrapping_sub(tm);
                if matches!(mapped, Some(Some((true, _)))) { file_name_from_fd(cap, pid, fd); }
                mmap_file_ref(cap, false); // drop the creation reference
                return match mapped {
                    Some(Some((true, at))) => at as isize,
                    _ => enomem_map_site("mmap/file-private"),
                };
            }
        }
    }

    // [GAP2] A MAP_SHARED file mapping that did NOT take the K1 aliasing branch
    // above is about to get an EAGER PRIVATE COPY (the read loop below). If the
    // Wayland shm-pool fd shows up here, that mapping is frozen at frame 0.
    if flags & MAP_SHARED != 0 && mm::gap2::budget_ok() {
        mm::gap2::s("[G2FALL] pid="); mm::gap2::h(pid as usize);
        mm::gap2::kv(" fd=",   fd);
        mm::gap2::kv(" kind=", vfs::gap2_kind_tag(&kind));
        mm::gap2::kv(" off=",  off);
        mm::gap2::kv(" len=",  len);
        mm::gap2::kv(" prot=", prot);
        mm::gap2::nl();
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
    //
    // The copy below goes through the kernel direct map, so the VMA is
    // installed with its final protection right away (no temporary WRITABLE
    // + mprotect fixup: on x86_64/TCG that second locked pass with its TLB
    // shootdown cost 0.5-1.7 s per map during session start).
    if eager_too_big { return -22; }
    tr.kind = 4;
    // Allocate and FILL the block before the VMA exists, then publish it.
    // The old order (map, drop the address-space lock, read the file into the
    // mapped block through the direct map) let a sibling thread's munmap of
    // the fresh range free the block mid-read: the rest of the VFS read then
    // wrote into a free -- or already reused -- buddy block. Unpublished, the
    // block is private to this call, so the read needs no lock at all.
    let tm = monotonic_ns();
    let phys = match mm::vmm::AddressSpace::alloc_prefill(len) {
        Some(p) => p,
        None    => return enomem_map_site("mmap/file-backed"),
    };
    tr.map_ns = monotonic_ns().wrapping_sub(tm);
    let block_len = len.div_ceil(mm::buddy::PAGE_SIZE) * mm::buddy::PAGE_SIZE;

    // Step 3: read file data into the physical pages.
    // We read up to `len` bytes; if the file is shorter, the rest is zeroed.
    // Loop rather than issuing one big read: several VFS read paths (tmpfs and
    // the pipe ring among them) cap a single reply at 4 KiB, so a one-shot read
    // silently left everything past the first page zeroed in any mapping larger
    // than that. Short reads are normal here, not EOF.
    let hhdm_ptr = mm::phys_to_virt(phys) as *mut u8;
    let mut filled: usize = 0;
    let mut n: isize = 0;
    let tio = monotonic_ns();
    //
    // Bounded chunks with an interrupt window between them. Syscalls run with
    // IRQs masked, and this copy is EAGER: a MAP_PRIVATE map of a whole file
    // reads all of it here. Rust binaries map their own executable to
    // symbolize a backtrace (cosmic-comp 33 MiB, cosmic-greeter-login 36 MiB)
    // and ld-musl maps libgallium's 23 MiB text segment the same way, so one
    // unbroken read was 1.1-1.4 s on x86_64/KVM and 2.3-3.7 s on x86_64/TCG
    // with no timer tick on that CPU -- the `[WDOG] ... cosmic-comp ... last
    // syscall 9` report. Nothing is held between chunks, so the window may
    // also reschedule; the CPU is simply no longer deaf for all of it.
    while filled < len {
        let want = (len - filled).min(MMAP_READ_CHUNK);
        let read_msg = make_vfs_msg(vfs::VFS_READ,
            &[fd as u64, (hhdm_ptr as usize + filled) as u64, want as u64]);
        let r = vfs_reply_val(&vfs::handle(&read_msg, pid));
        if r < 0 { n = r; break; }
        if r == 0 { break; } // genuine EOF; the rest of the mapping stays zero
        filled += (r as usize).min(want);
        irq_window();
    }
    if n >= 0 { n = filled as isize; }
    tr.io_ns += monotonic_ns().wrapping_sub(tio);
    tr.bytes = filled;

    // Restore the descriptor's original file position — mmap(2) leaves it alone.
    if saved_pos >= 0 {
        let restore = make_vfs_msg(vfs::VFS_LSEEK,
            &[fd as u64, saved_pos as u64, 0 /* SEEK_SET */]);
        let _ = vfs_reply_val(&vfs::handle(&restore, pid));
    }

    if n < 0 {
        mm::vmm::AddressSpace::free_prefill(phys, len);
        return n as isize;
    }
    // Everything past the file data reads as zero (the block is not pre-zeroed).
    unsafe { hhdm_ptr.add(filled).write_bytes(0, block_len - filled); }

    // AArch64: the copy went through the data side of the direct map; make
    // an executable mapping's bytes visible to instruction fetch.
    #[cfg(target_arch = "aarch64")]
    if page_flags.contains(PageFlags::EXECUTE) {
        unsafe {
            let mut line = hhdm_ptr as usize & !63;
            let end_a = hhdm_ptr as usize + len;
            while line < end_a {
                core::arch::asm!("dc cvau, {}", in(reg) line);
                line += 64;
            }
            core::arch::asm!("dsb ish", "ic ialluis", "dsb ish", "isb");
        }
    }

    // Publish. The copy went through the kernel direct map, so the VMA is
    // installed with its final protection right away (no temporary WRITABLE
    // + mprotect fixup: on x86_64/TCG that second locked pass with its TLB
    // shootdown cost 0.5-1.7 s per map during session start).
    let tm = monotonic_ns();
    // L1.13: a big MAP_FIXED destination goes in bounded holds first; the in-hold
    // unmap below then only sees empty VMAs.
    if flags & MAP_FIXED != 0 && len > UNMAP_CHUNK_PAGES * mm::buddy::PAGE_SIZE { unmap_chunked(virt, len); }
    let mapped = with_current_address_space_mut(|as_| {
        let at = if flags & MAP_FIXED != 0 {
            as_.unmap_range(virt, len);
            virt
        } else {
            mmap_place(as_, virt, len, place)?
        };
        if !as_.map_prefilled(at, len, page_flags, phys) { return None; }
        as_.set_prot(at, prot as u32);
        as_.mark_file_copy(at);
        Some(at)
    });
    tr.map_ns += monotonic_ns().wrapping_sub(tm);
    let virt = match mapped { Some(Some(at)) => at, _ => 0 };
    if virt == 0 {
        mm::vmm::AddressSpace::free_prefill(phys, len);
        return enomem_map_site("mmap/file-backed");
    }

    virt as isize
}

/// msync(addr, len, flags): write back what shared file mappings in the
/// range stored (the page cache's dirty pages of those files); MS_SYNC also
/// commits them (a checkpoint). Other mappings have nothing to write.
/// ENOMEM if part of the range is unmapped, as on Linux.
fn sys_msync(addr: usize, len: usize, flags: usize) -> isize {
    const MS_ASYNC: usize = 1;
    const MS_INVALIDATE: usize = 2;
    const MS_SYNC: usize = 4;
    if addr & (mm::buddy::PAGE_SIZE - 1) != 0 { return -22; }
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0 { return -22; }
    if flags & MS_ASYNC != 0 && flags & MS_SYNC != 0 { return -22; }
    let keys = match with_current_address_space(|as_| as_.shared_file_keys(addr, len)) {
        Some(Some(k)) => k,
        Some(None) => return -12,
        None => return -12,
    };
    let mut rc = 0isize;
    for key in keys {
        let r = f2fs_server::pcache_msync(key, flags & MS_SYNC != 0);
        if r < 0 { rc = r; }
    }
    rc
}

/// sys_unmap_mem(virt, size) — unmap and free the pages at `virt`.
fn sys_unmap_mem(virt: usize, size: usize) -> isize {
    if virt == 0 || size == 0 { return -22; } // EINVAL
    if virt >= USER_SPACE_END  { return -22; }

    if CHURN_STATS {
        let t0 = monotonic_ns();
        let (kind, pres) = with_current_address_space_mut(|as_| {
            let k = as_.find(virt).map(|r| {
                let kind = if r.file_cap == usize::MAX { 2 }
                    else if r.file_cap != 0 { 5 }
                    else if !r.lazy { 3 } else { 1 };
                (kind, r.lazy_count as u64)
            }).unwrap_or((0, 0));
            as_.unmap(virt, size);
            k
        }).unwrap_or((0, 0));
        churn_note(1, kind, size, 0, monotonic_ns().wrapping_sub(t0), pres);
        return 0;
    }
    unmap_chunked(virt, size);
    0
}

/// Pages released per address-space hold by [`unmap_chunked`] (8 MiB).
const UNMAP_CHUNK_PAGES: usize = 2048;

/// munmap without one unbounded IRQs-off hold: the resident pages of private
/// lazy mappings in the range are dropped [`UNMAP_CHUNK_PAGES`] at a time
/// (`strip_range`, one hold and one TLB shootdown each, an interrupt window
/// between holds, so siblings spinning on the busy flag get in too), then the
/// now nearly empty VMAs go in one short final `unmap`. A sibling thread
/// touching the range between holds sees the same thing as touching it just
/// before munmap began; nothing outside the range is affected.
fn unmap_chunked(virt: usize, size: usize) {
    let page = mm::buddy::PAGE_SIZE;
    let chunk = UNMAP_CHUNK_PAGES * page;
    if size > chunk {
        let end = virt.saturating_add(size);
        let mut at = virt & !(page - 1);
        while at < end {
            let n = (end - at).min(chunk);
            let _ = with_current_address_space_mut(|as_| {
                let t0 = sched::monotonic_ns();
                let r = as_.strip_range(at, n);
                sched::note_as_hold_ns(sched::monotonic_ns().wrapping_sub(t0));
                r
            });
            // The shootdown wait happens here, with the AS lock dropped.
            mm::vmm::drain_deferred(true);
            irq_window();
            at += n;
        }
    }
    with_current_address_space_mut(|as_| {
        let t0 = sched::monotonic_ns();
        as_.unmap(virt, size);
        sched::note_as_hold_ns(sched::monotonic_ns().wrapping_sub(t0));
    });
    mm::vmm::drain_deferred(true);
}

/// mremap(2), Linux semantics.
///
/// The mapping is never copied: growing in place extends the VMA, and a move
/// carries its page-table entries to the new range, so the frames and the
/// backing stay what they were — a shared mapping (shared anonymous memory, a
/// shared file, a tmpfs/memfd object) keeps sharing with every other mapper,
/// a private one keeps its pages and copy-on-write state. Until 2026-10-19 a
/// move of anything but a shared mapping of unchanged size was an anonymous
/// copy (a grown memfd mapping turned private, cut off from its writer) and
/// MREMAP_FIXED treated `new_addr` as a hint without unmapping it.
///
/// - Without MREMAP_MAYMOVE the mapping grows in place or the call fails
///   ENOMEM (musl's pthread_getattr_np() probes the main stack this way);
///   with it, a grow that does not fit moves to a free range.
/// - MREMAP_FIXED (needs MAYMOVE) moves to exactly `new_addr`, unmapping
///   whatever was there; the two ranges may not overlap (EINVAL).
/// - MREMAP_DONTUNMAP (needs MAYMOVE, equal sizes, private anonymous) moves
///   the pages and leaves the old range mapped and empty.
/// - Shrinking unmaps the tail (and with MREMAP_FIXED then moves).
/// - The old range must lie inside one mapping (EFAULT otherwise).
fn sys_mremap(
    old_addr: usize, old_size: usize, new_size: usize,
    flags: usize, new_addr: usize,
) -> isize {
    const MREMAP_MAYMOVE: usize = 1;
    const MREMAP_FIXED: usize = 2;
    const MREMAP_DONTUNMAP: usize = 4;
    const PAGE: usize = 4096;

    if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP) != 0 { return -22; }
    if flags & (MREMAP_FIXED | MREMAP_DONTUNMAP) != 0 && flags & MREMAP_MAYMOVE == 0 { return -22; }
    if old_addr & (PAGE - 1) != 0 || new_size == 0 { return -22; }
    // Linux duplicates a shared mapping for old_size == 0; not supported.
    if old_size == 0 { return -22; }
    let round = |n: usize| n.checked_add(PAGE - 1).map(|v| v & !(PAGE - 1));
    let (old_len, new_len) = match (round(old_size), round(new_size)) {
        (Some(o), Some(n)) => (o, n),
        _ => return -22,
    };
    let fixed = flags & MREMAP_FIXED != 0;
    let dontunmap = flags & MREMAP_DONTUNMAP != 0;
    if dontunmap && old_len != new_len { return -22; }
    if old_addr.checked_add(old_len).map_or(true, |e| e > USER_SPACE_END) { return -14; }
    if fixed {
        if new_addr & (PAGE - 1) != 0 { return -22; }
        match new_addr.checked_add(new_len) {
            Some(e) if e <= USER_SPACE_END => {}
            _ => return -22,
        }
        if new_addr < old_addr + old_len && new_addr + new_len > old_addr { return -22; }
    }

    if new_len > old_len && !vm_limits_admit(new_len - old_len, false) { return -12; } // RLIMIT_AS
    let src = match with_current_address_space(|a| a.mremap_source(old_addr, old_len)) {
        Some(Ok(s)) => s,
        Some(Err(e)) => return e,
        None => return -14,
    };

    // Shrink: the tail goes, and without MREMAP_FIXED that is all.
    if new_len < old_len {
        unmap_chunked(old_addr + new_len, old_len - new_len);
        if !fixed { return old_addr as isize; }
    }
    let cur_len = old_len.min(new_len);
    if new_len == old_len && !fixed && !dontunmap { return old_addr as isize; }

    // A tmpfs/memfd mapping's frames are installed at map time, so its new
    // pages are the object's own frames, taken here, before the address
    // space (VFS locks are never taken under its `busy`).
    let grow = new_len - cur_len;
    let tail: alloc::vec::Vec<usize> = if grow > 0 && src.vmo != 0 {
        match vfs::vmo_acquire_frames_by_id(src.vmo, src.vmo_off as usize + cur_len, grow) {
            Some(f) => f,
            None => return -12,
        }
    } else { alloc::vec::Vec::new() };

    // L1.13: the checks above are done, so empty a MREMAP_FIXED destination in
    // bounded holds before the single move hold.
    if fixed && new_len > UNMAP_CHUNK_PAGES * PAGE { unmap_chunked(new_addr, new_len); }
    let hold_t0 = monotonic_ns();
    let r = with_current_address_space_mut(|as_| -> isize {
        // A sibling thread may have changed the range while it was unlocked.
        match as_.mremap_source(old_addr, cur_len) {
            Ok(s) if s.vmo == src.vmo && s.vmo_off == src.vmo_off => {}
            Ok(_) => return -14,
            Err(e) => return e,
        }
        if !fixed && !dontunmap && grow > 0 {
            let r = as_.mremap_grow(old_addr, cur_len, new_len, &tail);
            if r >= 0 || r == -14 || flags & MREMAP_MAYMOVE == 0 { return r; }
        }
        let dest = if fixed {
            as_.unmap_range(new_addr, new_len);
            new_addr
        } else {
            match mmap_place(as_, 0, new_len, MmapCand::Any) { Some(a) => a, None => return -12 }
        };
        as_.mremap_move(old_addr, cur_len, dest, new_len, &tail, dontunmap)
    }).unwrap_or(-14);
    let hold_us = monotonic_ns().saturating_sub(hold_t0) / 1000;
    if hold_us > 5000 {
        extern "C" { fn arch_serial_putc(c: u8); fn print_number(n: u32); }
        let ps = |s: &str| for &b in s.as_bytes() { unsafe { arch_serial_putc(b) } };
        ps("[MREMAP] pages="); unsafe { print_number((cur_len / PAGE) as u32) };
        ps(" hold_us="); unsafe { print_number(hold_us as u32) };
        ps("\n");
    }
    if r < 0 { vfs::vmo_release_frames(&tail); }
    r
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
///   -1  (EPERM)   — caller is not root: this is a raw task-creation
///                   primitive (no exec, no credential setup), so only a
///                   privileged server may use it
fn sys_spawn(entry_va: usize, stack_va: usize, priority_raw: usize) -> isize {
    if !sched::cred::current_is_root() { return -1; }
    // Reject entries that point into the kernel half of the address space.
    if entry_va == 0 || entry_va >= USER_SPACE_END { return -22; }
    if stack_va  >= USER_SPACE_END                 { return -22; }

    let priority = priority_raw as i8;
    match spawn_user(entry_va, stack_va, priority) {
        Some(pid) => pid as isize,
        None      => enomem_site("spawn/run-queue-or-oom"),
    }
}

/// Encode a reaped child's status as a POSIX wait status.
///
/// `<sys/wait.h>`'s `WIFEXITED`/`WEXITSTATUS` macros expect the musl/Linux
/// layout, where a normal termination is `(code & 0xff) << 8` and the low 7
/// bits are a terminating signal (0 ⇒ exited normally). Writing the raw code
/// straight through — as this used to — makes `WIFEXITED(status)` read false
/// and `WEXITSTATUS(status)` read 0 for any exit code ≥ 128, so every caller
/// that inspects the status misreads it.
///
/// The encoding itself now lives in `sched::ExitStatus::wait_status`, because
/// only the exit path knows *which* half of the layout applies: encoding
/// every death as a normal exit — which is what this function did while the
/// scheduler tracked a bare `i32` and the signal path stored the shell's
/// `128 + signo` — made `WIFSIGNALED` read false for every killed process.
#[inline]
fn encode_wait_status(status: sched::ExitStatus) -> i32 {
    status.wait_status()
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
/// `__WNOTHREAD | __WCLONE | __WALL`: the Linux-only wait options.
const WAIT_LINUX_FLAGS: usize = 0x2000_0000 | 0x8000_0000 | 0x4000_0000;

/// Which children a wait looks at by exit signal: `__WALL` all, `__WCLONE`
/// the ones that report with a signal other than SIGCHLD (clone/clone3
/// exit_signal), otherwise the SIGCHLD ones (Linux `eligible_child`).
fn wait_clone_sel(options: usize) -> u8 {
    if options & 0x4000_0000 != 0 { sched::WAIT_ALL }
    else if options & 0x8000_0000 != 0 { sched::WAIT_CLONE }
    else { sched::WAIT_NORMAL }
}

fn sys_wait4(pid_raw: usize, status_ptr: usize, options: usize, rusage: usize) -> isize {
    // Validate before blocking — catches bad pointers before we yield.
    if status_ptr != 0 && !validate_user_ptr_aligned(status_ptr, core::mem::size_of::<i32>(), 4) {
        return -14;
    }
    const WNOHANG:    usize = 1;
    const WUNTRACED:  usize = 2;
    const WCONTINUED: usize = 8;
    let options = options as u32 as usize; // an `int`
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WAIT_LINUX_FLAGS) != 0 { return -22; }
    let what = sched::WaitWhat {
        exited:    true,
        stopped:   options & WUNTRACED != 0,
        continued: options & WCONTINUED != 0,
        clone:     wait_clone_sel(options),
    };
    // Packed wait status for a stopped child: `WIFSTOPPED` is `(s & 0xff) ==
    // 0x7f`, `WSTOPSIG` is `(s >> 8) & 0xff`. A continued child is the fixed
    // 0xffff (`WIFCONTINUED`). musl/relibc `<sys/wait.h>` encoding.
    let stopped_status = |sig: u32| -> i32 { ((sig as i32) << 8) | 0x7f };
    const CONTINUED_STATUS: i32 = 0xffff;
    let write_status = |status: i32| {
        if status_ptr != 0 {
            // Fault the destination in first: a demand-paged .bss
            // status variable would otherwise make write_user_buf
            // fail silently.
            prefault_user(status_ptr, 4);
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
    };
    // pid_t travels as a sign-extended long; truncating through u32 maps
    // both 0xFFFF_FFFF and 0xFFFF_FFFF_FFFF_FFFF to -1.
    let pid_i = pid_raw as u32 as i32;
    let sel = if pid_i == -1 {
        sched::WaitSel::Any
    } else if pid_i == 0 {
        sched::WaitSel::Pgid(sched::current_pgid())
    } else if pid_i < -1 {
        // Pid numbers are the caller's pid-namespace view; a child is never
        // the waiter namespace's init, so the values wait returns are
        // identity (lane nspid).
        match sched::pidns::from_cur((-(pid_i as i64)) as u32) { Ok(k) => sched::WaitSel::Pgid(k), Err(_) => return -10 }
    } else {
        match sched::pidns::from_cur(pid_i as u32) { Ok(k) => sched::WaitSel::Pid(k), Err(_) => return -10 }
    };
    let caller_tgid = sched::current_tgid();

    loop {
        match sched::wait_try(sel, caller_tgid, what) {
            sched::WaitTry::Reaped(pid, status, acct) => {
                pidfd_reaped_wake(pid);
                write_status(encode_wait_status(status));
                if rusage != 0 && write_rusage(rusage, &RusageVals::of_child(&acct)) != 0 { return -14; }
                return pid as isize;
            }
            sched::WaitTry::Stopped(pid, sig) => {
                write_status(stopped_status(sig));
                return pid as isize;
            }
            sched::WaitTry::Continued(pid) => {
                write_status(CONTINUED_STATUS);
                return pid as isize;
            }
            sched::WaitTry::NoChildren => return -10, // ECHILD
            sched::WaitTry::StillRunning => {
                if options & WNOHANG != 0 { return 0; }
                if interrupted() { return ERESTARTSYS; }
                // Block on the poll wait-channel instead of a yield-spin. A
                // child exit delivers SIGCHLD which calls sched::wake_poll,
                // waking us to reap; a short poll deadline bounds any missed
                // edge (check-then-sleep gap) to ~20 ms, never a hang. The old
                // `yield_now` spin pinned the reaper of a *long-running* child
                // (e.g. a compositor that runs for the session) at 100 % CPU —
                // init/shell waiters stayed perpetually runnable, churning the
                // scheduler run-loop and starving the very child's event loop.
                //
                // The park names this process (poll_class::WAIT): an exit
                // requests that tag as it is logged (`log_exit`), and a
                // SIGCHLD or any broadcast wake still reaches it. The deadline
                // is only a backstop now; at 20 ms it woke every idle waiter
                // (greetd, init's shells) 50 times a second for nothing.
                sched::block_on_poll_prepare_masked(
                    sched::monotonic_ns() + 1_000_000_000,
                    sched::poll_tag(sched::poll_class::WAIT, caller_tgid));
                if matches!(sched::wait_peek(sel, caller_tgid, what), sched::WaitTry::StillRunning)
                    && !interrupted() {
                    sched::block_on_poll_commit();
                } else {
                    sched::block_on_poll_cancel();
                }
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
    // Linux WNOWAIT: report the state change but leave the child waitable.
    const WNOWAIT:    usize = 0x0100_0000;
    let nowait = options & WNOWAIT != 0;
    // Linux requires at least one wait-state flag.
    if options & (WEXITED | WSTOPPED | WCONTINUED) == 0 { return -22; } // EINVAL
    // idtype: 0=P_ALL, 1=P_PID, 2=P_PGID, 3=P_PIDFD
    let mut pidfd_nonblock = false;
    let sel = match idtype {
        0 => sched::WaitSel::Any,
        1 => match sched::pidns::from_cur(id as u32) { Ok(k) => sched::WaitSel::Pid(k), Err(_) => return -10 },
        2 => match sched::pidns::from_cur(id as u32) { Ok(k) => sched::WaitSel::Pgid(k), Err(_) => return -10 },
        3 => {
            // The process the pidfd names; an O_NONBLOCK pidfd makes a wait
            // for a still-running child EAGAIN instead of blocking (Linux).
            let pid = match vfs::pidfd_target(current_pid(), id) { Ok(p) => p, Err(e) => return e as isize };
            pidfd_nonblock = vfs::fd_nonblock(current_pid(), id);
            sched::WaitSel::Pid(pid)
        }
        _ => return -22, // EINVAL
    };
    let caller_tgid = sched::current_tgid();
    // Without WEXITED the caller only wants stop/continue reports (e.g.
    // brush's poll_for_stopped_children uses WSTOPPED|WNOHANG) — exit
    // statuses must be left for a later wait4/WEXITED wait to collect.
    // `wait_scan` only consumes the kinds of report asked for here.
    let what = sched::WaitWhat {
        exited:    options & WEXITED != 0,
        stopped:   options & WSTOPPED != 0,
        continued: options & WCONTINUED != 0,
        clone:     wait_clone_sel(options as u32 as usize),
    };

    // Fill siginfo_t (si_signo=SIGCHLD at +0, si_code at +8, si_pid at +16,
    // si_status at +24). Built as a kernel-local buffer and copied out via
    // write_user_buf (virt->phys/HHDM), not a raw dereference of `infop`:
    // this syscall runs in kernel context, and the target page may be a
    // CoW-shared, PTE-read-only page the current process's own fault handler
    // never gets a chance to promote in that context.
    let write_info = |pid: u32, si_code: i32, si_status: i32| {
        if infop != 0 && validate_user_buf(infop, 128) {
            let mut buf = [0u8; 128];
            if pid != 0 {
                buf[0..4].copy_from_slice(&17i32.to_ne_bytes());   // si_signo = SIGCHLD
                buf[8..12].copy_from_slice(&si_code.to_ne_bytes());
                buf[16..20].copy_from_slice(&pid.to_ne_bytes());   // si_pid
                buf[24..28].copy_from_slice(&si_status.to_ne_bytes());
            }
            copy_to_user_bytes(infop, &buf);
        }
    };

    loop {
        let r = if nowait { sched::wait_peek(sel, caller_tgid, what) } else { sched::wait_try(sel, caller_tgid, what) };
        match r {
            sched::WaitTry::Reaped(pid, status, _) => {
                // si_code and si_status both come from ExitStatus so a waitid
                // caller that switches on CLD_KILLED reaches the same verdict
                // as one that decodes wait4's packed status. si_code was
                // hardcoded to CLD_EXITED here, which is the half of that
                // disagreement this file owned.
                if !nowait { pidfd_reaped_wake(pid); }
                write_info(pid, status.si_code(), status.si_status());
                return 0;
            }
            sched::WaitTry::Stopped(pid, sig) => {
                write_info(pid, sched::CLD_STOPPED, sig as i32);
                return 0;
            }
            sched::WaitTry::Continued(pid) => {
                write_info(pid, sched::CLD_CONTINUED, 18); // si_status = SIGCONT
                return 0;
            }
            sched::WaitTry::NoChildren => return -10, // ECHILD
            sched::WaitTry::StillRunning => {
                if options & WNOHANG != 0 {
                    // "no state change" — zeroed si_pid
                    write_info(0, 0, 0);
                    return 0;
                }
                if pidfd_nonblock { return -11; }
                if interrupted() { return ERESTARTSYS; }
                // Block on the poll wait-channel rather than a yield-spin —
                // same fix as sys_wait4: a long-running child (a compositor,
                // a service) otherwise pins its blocking-waitid reaper at
                // 100 % CPU. Woken by child-exit SIGCHLD -> wake_poll; the
                // 2-tick poll deadline bounds a missed edge to ~20 ms.
                // Same park as wait4 (see there): tagged, 1 s backstop.
                sched::block_on_poll_prepare_masked(
                    sched::monotonic_ns() + 1_000_000_000,
                    sched::poll_tag(sched::poll_class::WAIT, caller_tgid));
                let peek = sched::wait_peek(sel, caller_tgid, what);
                if !matches!(peek, sched::WaitTry::StillRunning) || interrupted() {
                    sched::block_on_poll_cancel();
                } else {
                    sched::block_on_poll_commit();
                }
            }
        }
    }
}

/// sys_rt_sigsuspend(mask_ptr, sigsetsize) — atomically set signal mask and pause.
///
/// Replaces the current signal mask, then yields until any unmasked signal
/// arrives.  Always returns -EINTR.
fn sys_rt_sigsuspend(mask_ptr: usize, _sigsetsize: usize) -> isize {
    if mask_ptr == 0 || !validate_user_buf(mask_ptr, 8) { return -14; } // EFAULT
    let new_mask = ur::<u64>(mask_ptr);
    sigsuspend_with(new_mask)
}

/// pause(2): sigsuspend under the CURRENT mask. It used to be
/// `rt_sigsuspend(NULL)`, which installed an all-clear mask — a pause()
/// with SIGALRM blocked woke on it and ran the handler.
fn sys_pause() -> isize {
    let cur = replace_signal_mask(0);
    replace_signal_mask(cur);
    sigsuspend_with(cur)
}

fn sigsuspend_with(new_mask: u64) -> isize {
    let old_mask = replace_signal_mask(new_mask);
    // Park until a signal arrives that is not blocked by new_mask. Every
    // delivery path (`deliver_signal`, `deliver_signal_process`) ends in a
    // `wake_poll` broadcast, so the poll wait-channel is the wake source;
    // three-phase, re-checking the pending set after publishing Blocked.
    // Thread-directed and process-directed instances both count (the latter
    // sit on the leader until the syscall-exit pass claims them).
    let wakeable = || (pending_signals() | sched::shared_pending_signals()) & !new_mask != 0;
    loop {
        if wakeable() { break; }
        sched::block_on_poll_prepare_masked(u64::MAX, sched::signal_wake_tag(sched::current_tgid()));
        if wakeable() { sched::block_on_poll_cancel(); break; }
        sched::block_on_poll_commit();
    }
    // The old mask is NOT put back here: the signal that ended the wait is
    // unblocked only under `new_mask`, and it is delivered on this syscall's
    // return to user space — restoring first would leave it pending and
    // blocked, with the handler never run. `check_and_deliver_signals`
    // consumes the saved mask: into the handler frame's `uc_sigmask`, or
    // straight back into `signal_mask` when nothing was deliverable after all.
    sched::stash_sigsuspend_mask(old_mask);
    -4 // EINTR
}

/// sys_rt_sigtimedwait(set_ptr, info_ptr, timeout_ptr, sigsetsize)
/// Waits until a signal in `set` is pending, or timeout elapses.
fn sys_rt_sigtimedwait(set_ptr: usize, info_ptr: usize, timeout_ptr: usize, _sz: usize) -> isize {
    let wait_mask: u64 = if set_ptr != 0 && validate_user_buf(set_ptr, 8) {
        ur::<u64>(set_ptr)
    } else { !0u64 };

    // Absolute monotonic deadline from the relative timespec ({0,0} = poll
    // once: the deadline is "now" and the check below fires at once).
    let deadline = if timeout_ptr != 0 && validate_user_buf(timeout_ptr, 16) {
        match read_user_timespec(timeout_ptr) {
            Ok(ns) => Some(deadline_after_ns(ns)),
            Err(e) => return e,
        }
    } else {
        None // no timeout — wait indefinitely
    };

    loop {
        // A process-directed signal that every thread blocks — the normal
        // sigwait pattern — is parked on the leader, not on this thread.
        let pending = (pending_signals() | sched::shared_pending_signals()) & wait_mask;
        if pending != 0 {
            let signo = pending.trailing_zeros() as u32 + 1;
            // Accept the signal, taking its payload with the pending bit.
            let info = sched::accept_pending_signal(signo);
            // Optionally fill siginfo_t (128 bytes) with the signal-frame
            // builder's own writer, so each class (SI_QUEUE's pid/uid/sigval,
            // SI_TIMER's tid/overrun/sigval, a child's status, a fault's
            // address) has one layout. SI_QUEUE's si_value used to be dropped.
            if info_ptr != 0 && validate_user_buf(info_ptr, 128) {
                let mut buf = [0u8; 128];
                sched::signal::write_siginfo(&mut buf, 0, signo, info);
                unsafe { ua::copy_to_user(info_ptr, buf.as_ptr(), 128); }
            }
            return signo as isize;
        }
        // Any OTHER signal that will run a handler (or kill) ends the wait
        // with EINTR, as on Linux; the waited-for set is checked first above.
        if interrupted() { return -4; }
        if let Some(dl) = deadline {
            // EAGAIN: POSIX and Linux report an expired wait this way, not ETIMEDOUT.
            if monotonic_ns() >= dl { return -11; }
        }
        // Park (see sys_rt_sigsuspend); the deadline rides the poll tick.
        sched::block_on_poll_prepare_masked(deadline.unwrap_or(u64::MAX), sched::signal_wake_tag(sched::current_tgid()));
        if (pending_signals() | sched::shared_pending_signals()) & wait_mask != 0 || interrupted() {
            sched::block_on_poll_cancel(); continue;
        }
        sched::block_on_poll_commit();
    }
}

/// Monotonic nanoseconds since boot, with sub-tick resolution.
///
/// The 100 Hz tick counter alone is a 10 ms-granular clock, and userspace acts
/// on the difference rather than just reading it: Mesa's venus ring suppresses
/// its "wake the idle renderer" notification whenever this clock says less than
/// 1 ms has passed since the last one, so a 10 ms clock silently withholds a
/// wake-up for up to 10 ms — while virglrenderer's ring thread re-idles after
/// 1 ms and, once idle, waits only on that notification and never re-checks the
/// ring. That is the whole `vktest`-under-TCG hang.
///
/// This is `sched::monotonic_ns()`: the ONE monotonic source. Every timed wait
/// in this file (nanosleep, poll/ppoll/select/epoll_wait, FUTEX_WAIT,
/// rt_sigtimedwait, timerfd) expresses its deadline as an absolute reading of
/// this clock, and the poll-deadline tick compares against the same reading.
/// They used to be tick counts: a relative timeout was floored to whole ticks
/// (any wait under 10 ms became 0 and returned at once) and a deadline of
/// "tick N" was released at N's edge — up to 10 ms before the requested
/// interval had elapsed by `clock_gettime`'s reckoning.
#[inline]
fn monotonic_ns() -> u64 { sched::monotonic_ns() }

/// CLOCK_REALTIME: the ONE wall clock (see `sched::realtime_ns`). Read from
/// the board's battery clock once at boot (`time_init`), steppable with
/// clock_settime/settimeofday, and otherwise `monotonic_ns` plus a constant.
#[inline]
fn realtime_ns() -> u64 { sched::realtime_ns() }

const CLOCK_REALTIME:           usize = 0;
const CLOCK_MONOTONIC:          usize = 1;
const CLOCK_PROCESS_CPUTIME_ID: usize = 2;
const CLOCK_THREAD_CPUTIME_ID:  usize = 3;
const CLOCK_MONOTONIC_RAW:      usize = 4;
const CLOCK_REALTIME_COARSE:    usize = 5;
const CLOCK_MONOTONIC_COARSE:   usize = 6;
const CLOCK_BOOTTIME:           usize = 7;

/// Read a user `struct timespec { tv_sec: i64, tv_nsec: i64 }` as nanoseconds.
/// `Err(-EFAULT)` for a bad pointer, `Err(-EINVAL)` for a negative field or
/// `tv_nsec >= 1e9` (the checks POSIX requires of every timespec consumer).
fn read_user_timespec(ptr: usize) -> Result<u64, isize> {
    if !validate_user_buf(ptr, 16) { return Err(-14); }
    let [tv_sec, tv_nsec] = match ua::read_user::<[i64; 2]>(ptr) { Some(v) => v, None => return Err(-14) };
    if tv_sec < 0 || tv_nsec < 0 || tv_nsec >= 1_000_000_000 { return Err(-22); }
    Ok((tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(tv_nsec as u64))
}

/// Write nanoseconds as a user `struct timespec`; false on a bad pointer.
fn write_user_timespec(ptr: usize, ns: u64) -> bool {
    ua::write_user::<[i64; 2]>(ptr, [(ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64])
}

/// Absolute monotonic deadline `rel_ns` from now. Saturates at `u64::MAX`,
/// which the park API reads as "no deadline" — a 584-year timeout is one.
#[inline]
fn deadline_after_ns(rel_ns: u64) -> u64 { monotonic_ns().saturating_add(rel_ns) }

#[inline]
fn deadline_after_ms(ms: u64) -> u64 { deadline_after_ns(ms.saturating_mul(1_000_000)) }

/// The clock the filesystems stamp inodes with (`sched::clock_ns`, wired at
/// boot via `sched::register_clock_ns`): CLOCK_REALTIME, not monotonic, so
/// `utimensat`'s UTIME_NOW and `stat`'s reported times agree with what
/// `clock_gettime(CLOCK_REALTIME)` hands userspace (the same wall clock
/// `set_realtime_ns`/`clock_settime`/`settimeofday` step).
pub fn clock_ns_for_sched() -> u64 { realtime_ns() }

/// sys_clock_gettime(clkid, tp_ptr).
///
/// CLOCK_REALTIME (and _COARSE) is the wall clock; the CPU-time clocks are
/// real (see `Task::cpu_ns`); MONOTONIC, MONOTONIC_RAW, MONOTONIC_COARSE and
/// BOOTTIME are all `monotonic_ns` (nothing here suspends, and the raw clock
/// is not slewed). Unknown ids are EINVAL, as on Linux.
fn sys_clock_gettime(clkid: usize, tp_ptr: usize) -> isize {
    if !validate_user_ptr_aligned(tp_ptr, 16, 8) { return -14; }
    let ns = match clkid {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => realtime_ns(),
        CLOCK_PROCESS_CPUTIME_ID => sched::process_cpu_ns(sched::current_tgid()),
        CLOCK_THREAD_CPUTIME_ID  => sched::thread_cpu_ns(current_pid()),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => monotonic_ns(),
        _ => return -22, // EINVAL
    };
    if !write_user_timespec(tp_ptr, ns) { return -14; }
    0
}

/// A child was reaped: a pidfd naming it gains POLLHUP (`poll_vnode`'s PidFd
/// arm reports `reaped`), so its pollers are woken on the pidfd's tag, as the
/// exit itself wakes them (invariant 16). Before, a registration waiting for
/// the hang-up alone (no POLLIN) slept through the reap.
fn pidfd_reaped_wake(pid: u32) {
    sched::wake_poll_key(sched::poll_key(sched::poll_class::PIDFD, pid));
}

/// sys_clock_settime(clkid, tp_ptr) — step the wall clock (root only). Only
/// CLOCK_REALTIME is settable; the monotonic and CPU clocks are EINVAL.
fn sys_clock_settime(clkid: usize, tp_ptr: usize) -> isize {
    if clkid != CLOCK_REALTIME { return -22; }
    let ns = match read_user_timespec(tp_ptr) { Ok(n) => n, Err(e) => return e };
    if sched::current_euid() != 0 { return -1; } // EPERM
    step_realtime_ns(ns);
    0
}

/// Step the wall clock (clock_settime / settimeofday) and tell the absolute
/// CLOCK_REALTIME timerfds: they follow the wall instant, and the
/// TFD_TIMER_CANCEL_ON_SET ones are cancelled and woken (Linux
/// `clock_was_set` → `timerfd_clock_was_set`).
fn step_realtime_ns(ns: u64) {
    let old = sched::realtime_offset_ns();
    sched::set_realtime_ns(ns);
    let new = sched::realtime_offset_ns();
    vfs::timerfd_clock_was_set(old, new);
    // Absolute CLOCK_REALTIME POSIX timers, and the threads parked until a
    // wall-clock instant (FUTEX_WAIT_BITSET|FUTEX_CLOCK_REALTIME,
    // clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME)), follow the step: each
    // still expires at its wall-clock instant (Linux `clock_was_set`).
    tty_server::timers_clock_was_set(old, new);
    sched::realtime_waiters_clock_was_set();
    // Raspberry Pi 5: keep the firmware RTC in step (Linux rtc-rpi via hwclock -w).
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    drivers::rpi_misc::rtc_set_secs(ns / 1_000_000_000);
}

/// sys_settimeofday(tv_ptr, tz_ptr) — step the wall clock from a `struct
/// timeval` (root only). The timezone argument is accepted and ignored.
fn sys_settimeofday(tv_ptr: usize, _tz_ptr: usize) -> isize {
    if tv_ptr == 0 { return 0; }
    if !validate_user_buf(tv_ptr, 16) { return -14; }
    let tv_sec  = match sched::uaccess::read_user::<i64>(tv_ptr) { Some(v) => v, None => return -14 };
    let tv_usec = match sched::uaccess::read_user::<i64>(tv_ptr + 8) { Some(v) => v, None => return -14 };
    if tv_sec < 0 || tv_usec < 0 || tv_usec >= 1_000_000 { return -22; }
    if sched::current_euid() != 0 { return -1; } // EPERM
    step_realtime_ns((tv_sec as u64).saturating_mul(1_000_000_000) + (tv_usec as u64) * 1_000);
    0
}

/// sys_getrandom(buf, count, flags) — bytes from the kernel CSPRNG
/// (`sched::random`: ChaCha20, fast key erasure, seeded at boot from the
/// CPU's hardware RNG where present plus timing jitter).
///
/// Flags follow Linux: GRND_NONBLOCK (1), GRND_RANDOM (2), GRND_INSECURE (4);
/// anything else, or GRND_INSECURE together with GRND_RANDOM, is EINVAL. The
/// generator is seeded before the first user process runs, so there is no
/// "not yet initialised" state to block on or report EAGAIN for, and
/// GRND_RANDOM draws from the same generator (Linux >= 5.6 semantics). A
/// single call returns at most 32 MiB - 1 bytes, as Linux does.
fn sys_getrandom(buf_ptr: usize, count: usize, flags: usize) -> isize {
    const GRND_NONBLOCK: usize = 1;
    const GRND_RANDOM:   usize = 2;
    const GRND_INSECURE: usize = 4;
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 { return -22; }
    if flags & GRND_INSECURE != 0 && flags & GRND_RANDOM != 0 { return -22; }
    let count = count.min((32 << 20) - 1);
    if count == 0 { return 0; }
    if !validate_user_buf(buf_ptr, count) { return -14; }
    // Generate into a kernel buffer (the generator's lock is never held
    // across a user-memory access), then copy out.
    let mut chunk = [0u8; sched::random::CHUNK];
    let mut done = 0;
    while done < count {
        let n = (count - done).min(chunk.len());
        sched::random::fill(&mut chunk[..n]);
        prefault_user(buf_ptr + done, n);
        if unsafe { ua::copy_to_user(buf_ptr + done, chunk.as_ptr(), n) } != 0 { return if done > 0 { done as isize } else { -14 }; }
        done += n;
    }
    for b in chunk.iter_mut() { unsafe { core::ptr::write_volatile(b, 0); } }
    count as isize
}

/// sys_prctl(option, arg2..5) — process control.
///
/// PR_SET_NAME (15) / PR_GET_NAME (16): per-thread name (sched::set_comm/comm_of).
/// All others: return 0 (silently ignore).
fn sys_prctl(option: usize, arg2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    const PR_GET_SECCOMP: usize = 21;
    const PR_SET_SECCOMP: usize = 22;
    const PR_SET_NO_NEW_PRIVS: usize = 38;
    const PR_GET_NO_NEW_PRIVS: usize = 39;
    const PR_CAPBSET_READ: usize = 23;
    const PR_CAPBSET_DROP: usize = 24;
    const PR_SET_NAME: usize = 15;
    const PR_GET_NAME: usize = 16;
    const PR_SET_DUMPABLE: usize = 4;
    const PR_GET_DUMPABLE: usize = 3;
    const PR_GET_AUXV: usize = 0x4155_5856; // "AUXV"
    const PR_SET_MM: usize = 35;
    const PR_SET_PDEATHSIG: usize = 1;
    const PR_GET_PDEATHSIG: usize = 2;
    match option {
        PR_SET_NAME => {
            // Up to 15 bytes of the NUL-terminated user string (Linux
            // truncates silently); shared with /proc/<tid>/comm.
            if !validate_user_buf(arg2, 16) { return -14; }
            let mut tmp = [0u8; 16];
            if unsafe { ua::copy_from_user(tmp.as_mut_ptr(), arg2, 16) } != 0 { return -14; }
            sched::set_comm(sched::current_pid(), &tmp[..15]);
            0
        }
        PR_GET_NAME => {
            // Write a 16-byte NUL-padded thread name.
            if validate_user_buf(arg2, 16) {
                let mut nm = [0u8; 15];
                let n = sched::comm_of(sched::current_pid(), &mut nm);
                let mut out = [0u8; 16];
                out[..n].copy_from_slice(&nm[..n]);
                if unsafe { ua::copy_to_user(arg2, out.as_ptr(), 16) } != 0 { return -14; }
            }
            0
        }
        // PR_SET_MM (35): only the argv/envp bounds are implemented (what
        // procfs reads; setproctitle libraries that move the title off the
        // original stack use them). The other fields are still silently
        // accepted, as before. Linux: arg5 must be 0, and arg4 too except
        // for the AUXV/MAP options (prctl_set_mm).
        PR_SET_MM => {
            use sched::MmArgField as F;
            let field = match arg2 {
                8 => F::ArgStart, 9 => F::ArgEnd, 10 => F::EnvStart, 11 => F::EnvEnd,
                _ => return 0,
            };
            if a4 != 0 || a5 != 0 { return -22; }
            sched::set_mm_arg_field(field, a3)
        }
        // Linux accepts only SUID_DUMP_DISABLE (0) and SUID_DUMP_USER (1)
        // here. The flag gates the ptrace-attach check (pidfd_getfd).
        PR_SET_DUMPABLE => match arg2 { 0 | 1 => { sched::set_dumpable(arg2 == 1); 0 } _ => -22 },
        // The signal this process gets when the thread that created it
        // exits (sched::thread_children_on_exit / reparent_children).
        PR_SET_PDEATHSIG => {
            if arg2 > 64 { return -22; }
            sched::set_pdeath_signal(arg2 as u8);
            0
        }
        PR_GET_PDEATHSIG => {
            let v = (sched::pdeath_signal() as i32).to_ne_bytes();
            if !validate_user_buf(arg2, 4) { return -14; }
            if unsafe { sched::uaccess::copy_to_user(arg2, v.as_ptr(), 4) } != 0 { return -14; }
            0
        }
        PR_GET_DUMPABLE => sched::dumpable() as isize,
        // PR_GET_AUXV (Linux 6.4): EINVAL, as on older kernels. Answering 0
        // ("success, 0 bytes") made rustix take an EMPTY aux vector as the
        // real one, so its page_size() was 0 and bottom showed every
        // process's memory as 0 B. On EINVAL it reads /proc/self/auxv.
        PR_GET_AUXV => -22,
        // Accepted silently before set-ID exec existed; now that execve can
        // grant privileges it has to be honoured (exec_setid). Linux: only
        // the value 1 with the remaining arguments zero, and never unset.
        PR_SET_NO_NEW_PRIVS => {
            if arg2 != 1 || a3 != 0 || a4 != 0 || a5 != 0 { return -22; }
            sched::set_no_new_privs();
            0
        }
        PR_GET_NO_NEW_PRIVS => {
            if arg2 != 0 || a3 != 0 || a4 != 0 || a5 != 0 { return -22; }
            sched::no_new_privs() as isize
        }
        PR_CAPBSET_READ => match sched::caps::caps_of(0) {
            Some(c) if arg2 <= sched::caps::CAP_LAST as usize => (c[3] >> arg2 & 1) as isize,
            _ => -22,
        },
        PR_CAPBSET_DROP => sched::caps::bset_drop(arg2.min(255) as u32),
        PR_GET_SECCOMP => sched::seccomp::current_mode() as isize,
        PR_SET_SECCOMP => match arg2 {
            1 if a3 == 0 => sched::seccomp::set_mode_strict(),
            2 => sched::seccomp::set_mode_filter(0, a3),
            _ => -22,
        },
        _ => 0, // silently accept anything else
    }
}

/// seccomp(2): STRICT, FILTER (no TSYNC/NEW_LISTENER yet) and GET_ACTION_AVAIL.
fn sys_seccomp(op: usize, flags: usize, args: usize) -> isize {
    match op {
        0 => if flags == 0 && args == 0 { sched::seccomp::set_mode_strict() } else { -22 },
        1 => sched::seccomp::set_mode_filter(flags as u32, args),
        2 => {
            if flags != 0 || !validate_user_buf(args, 4) { return -22; }
            let mut a = [0u8; 4];
            if unsafe { sched::uaccess::copy_from_user(a.as_mut_ptr(), args, 4) } != 0 { return -14; }
            sched::seccomp::action_avail(u32::from_ne_bytes(a))
        }
        _ => -22,
    }
}

/// Resolution of the clock `sys_clock_gettime` reports, in nanoseconds.
#[inline]
fn clock_resolution_ns() -> u64 {
    #[cfg(target_arch = "x86_64")]
    { arch_x86_64::timer::resolution_ns() }
    #[cfg(target_arch = "aarch64")]
    { arch_aarch64::timer::resolution_ns() }
}

/// sys_clock_getres(clkid, res_ptr) — return the resolution of a clock.
///
/// This must track `sys_clock_gettime`, not the tick rate: reporting 10 ms
/// while the clock interpolates to the counter period tells callers that
/// timestamps closer together than 10 ms are indistinguishable, which is
/// exactly the false belief that stalled Mesa's ring notification.
/// Always under a second, so the seconds field is zero by construction.
fn sys_clock_getres(_clkid: usize, res_ptr: usize) -> isize {
    if res_ptr != 0 {
        if !validate_user_buf(res_ptr, 16) { return -14; }
        if !ua::write_user::<[i64; 2]>(res_ptr, [0i64, clock_resolution_ns() as i64]) { return -14; }
    }
    0
}

/// sys_pread64(fd, buf, count, offset) — read from `fd` at `offset` without changing pos.
fn sys_pread64(fd: usize, buf_ptr: usize, count: usize, offset: usize) -> isize {
    if count == 0 { return 0; }
    if !validate_user_buf(buf_ptr, count) { return -14; }
    if !user_buf_writable(buf_ptr, count) { return -14; }
    // As for read(2): the destination must be resident and private before it
    // reaches f2fs (F2FS_MOUNTS held), a pipe ring or a pty — a fault there on
    // a lazy private-file page would re-enter the filesystem and deadlock.
    prefault_user(buf_ptr, count);
    pread_at(current_pid(), fd, buf_ptr, count, offset)
}

/// The positional read behind pread64(2), on an already validated (user or
/// kernel) buffer — splice(2) with an `off_in` reads into a kernel buffer.
fn pread_at(pid: u32, fd: usize, buf_ptr: usize, count: usize, offset: usize) -> isize {
    if (offset as isize) < 0 { return -22; }
    // A file on a mounted volume: a real positional read (through the page
    // cache), the descriptor's offset untouched — threads sharing a
    // descriptor (SQLite) no longer race on a seek/read/seek-back.
    if let Some((port, file_id, flags)) = vfs::mounted_file_of(pid, fd) {
        const O_PATH: u32 = 0o10000000;
        if flags & O_PATH != 0 { return -9; }
        return f2fs_server::pread_fd(port, file_id as u64, buf_ptr as *mut u8, count, offset as u64);
    }
    // A block-device node: positional in the VFS itself, so threads sharing
    // the descriptor cannot interleave their seeks (e2fsprogs reads bitmaps
    // from several threads on one fd).
    {
        let m = make_vfs_msg(vfs::VFS_PREAD, &[fd as u64, buf_ptr as u64, count as u64, offset as u64]);
        let r = vfs_reply_val(&vfs::handle(&m, pid));
        if r as i64 != vfs::ERR_NOT_POSITIONAL { return r; }
    }
    // Seek to offset, read, seek back (best-effort; position state is in VFS).
    // The position to go back to is the one before the seek: restoring the
    // seek's own result (== `offset`) used to leave every tmpfs pread with
    // its descriptor moved to the read offset.
    let cur_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, 0, 1 /* SEEK_CUR */]);
    let cur = vfs_reply_val(&vfs::handle(&cur_msg, pid));
    if cur < 0 { return cur; }
    let seek_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, offset as u64, 0 /* SEEK_SET */]);
    let at = vfs_reply_val(&vfs::handle(&seek_msg, pid));
    if at < 0 { return at; }
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
    // As for write(2): see sys_pread64.
    prefault_user_ro(buf_ptr, count);
    pwrite_at(current_pid(), fd, buf_ptr, count, offset, 0)
}

/// The positional write behind pwrite64(2), on an already validated (user or
/// kernel) buffer — see `pread_at`.
///
/// `ap` is the per-call append override of pwritev2: 0 follows the descriptor's
/// O_APPEND, 1 (RWF_NOAPPEND) writes at `offset` even on an O_APPEND descriptor,
/// 2 (RWF_APPEND) writes at end-of-file whatever the descriptor says.
fn pwrite_at(pid: u32, fd: usize, buf_ptr: usize, count: usize, offset: usize, ap: u64) -> isize {
    if (offset as isize) < 0 { return -22; }
    // A file on a mounted volume: positional (see sys_pread64). An O_APPEND
    // descriptor appends, as on Linux.
    if let Some((port, file_id, flags)) = vfs::mounted_file_of(pid, fd) {
        const O_PATH: u32 = 0o10000000;
        const O_APPEND: u32 = 0o2000;
        if flags & O_PATH != 0 { return -9; }
        return f2fs_server::pwrite_fd(port, file_id as u64, buf_ptr as *const u8, count, offset as u64,
                                      match ap { 1 => false, 2 => true, _ => flags & O_APPEND != 0 }, pid);
    }
    // A block-device node: positional in the VFS itself, so threads sharing
    // the descriptor cannot interleave their seeks (e2fsprogs reads bitmaps
    // from several threads on one fd).
    {
        let m = make_vfs_msg(vfs::VFS_PWRITE, &[fd as u64, buf_ptr as u64, count as u64, offset as u64]);
        let r = vfs_reply_val(&vfs::handle(&m, pid));
        if r as i64 != vfs::ERR_NOT_POSITIONAL { return r; }
    }
    // Get current position.
    let cur_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, 0u64, 1 /* SEEK_CUR */]);
    let cur = vfs_reply_val(&vfs::handle(&cur_msg, pid));
    if cur < 0 { return if fd <= 2 && !vfs::fd_redirected(pid, fd) { count as isize } else { cur }; }
    // Seek to target offset.
    let seek_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, offset as u64, 0]);
    let _ = vfs::handle(&seek_msg, pid);
    // Write.
    let write_msg = make_vfs_msg(vfs::VFS_WRITE, &[fd as u64, buf_ptr as u64, count as u64, ap]);
    let n = vfs_reply_val(&vfs::handle(&write_msg, pid));
    // Restore position.
    let back_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, cur as u64, 0]);
    let _ = vfs::handle(&back_msg, pid);
    n
}

// ── preadv/pwritev/preadv2/pwritev2 ──────────────────────────────────────────

/// `RWF_*` flags (include/uapi/linux/fs.h) that preadv2/pwritev2 accept.
/// HIPRI is a polling hint (a no-op here); DSYNC/SYNC make the write durable
/// like O_DSYNC/O_SYNC; NOWAIT is the per-call "never block". The later flags
/// (ATOMIC, DONTCACHE) get EOPNOTSUPP, which is what a
/// kernel that predates them answers and what callers probe for.
const RWF_HIPRI: usize = 0x1;
const RWF_DSYNC: usize = 0x2;
const RWF_SYNC: usize = 0x4;
const RWF_NOWAIT: usize = 0x8;
/// APPEND writes at end-of-file for this call; NOAPPEND writes at the given
/// offset even on an O_APPEND descriptor (musl 1.2.6 pwrite() sends it first).
const RWF_APPEND: usize = 0x10;
const RWF_NOAPPEND: usize = 0x20;
const RWF_SUPPORTED: usize = RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_NOWAIT | RWF_APPEND | RWF_NOAPPEND;

/// One `struct iovec` of an already validated vector.
fn iov_at(iov_ptr: usize, i: usize) -> (usize, usize) {
    let [base, len] = ur::<[usize; 2]>(iov_ptr + i * 16);
    (base, len)
}

/// The vector checks of Linux `import_iovec`: EINVAL past UIO_MAXIOV or when
/// the lengths overflow ssize_t, EFAULT for an unreadable vector.
fn check_iov(iov_ptr: usize, iovcnt: usize) -> isize {
    const UIO_MAXIOV: usize = 1024;
    if iovcnt > UIO_MAXIOV { return -22; }
    if iovcnt == 0 { return 0; }
    if !user_readable(iov_ptr, iovcnt * 16) { return -14; }
    let mut total: usize = 0;
    for i in 0..iovcnt {
        let (_, len) = iov_at(iov_ptr, i);
        total = match total.checked_add(len) {
            Some(t) if t <= isize::MAX as usize => t,
            _ => return -22,
        };
    }
    0
}

/// One read or write of `len` bytes at `base` on `fd` that never blocks: a
/// socket gets MSG_DONTWAIT, a VFS object one VFS_READ/VFS_WRITE whose EAGAIN
/// is returned instead of waited on (RWF_NOWAIT at the file position).
fn rw_nowait(fd: usize, base: usize, len: usize, write: bool) -> isize {
    const MSG_DONTWAIT: usize = 0x40;
    if is_sock_handle(fd) {
        return if write { sys_sendto(fd, base, len, MSG_DONTWAIT, 0, 0) }
               else { sys_recvfrom(fd, base, len, MSG_DONTWAIT, 0, 0) };
    }
    let op = if write { vfs::VFS_WRITE } else { vfs::VFS_READ };
    let msg = make_vfs_msg(op, &[fd as u64, base as u64, len as u64]);
    vfs_reply_val(&vfs::handle(&msg, current_pid()))
}

/// preadv(2) (`v2 == false`) and preadv2(2). The offset is the 64-bit `pos_l`
/// (`pos_h` only matters to 32-bit ABIs). preadv2 with offset -1 reads at, and
/// advances, the file position like readv(2); any other negative offset is
/// EINVAL. RWF_NOWAIT on an object that cannot honour it (a terminal, the
/// console, a device) is EOPNOTSUPP, as on Linux, whose tty files lack
/// FMODE_NOWAIT. Bun's non-blocking stdin reader issues
/// `preadv2(0, .., -1, RWF_NOWAIT)` and falls back to poll + read on exactly
/// that error; ENOSYS left the Claude Code TUI deaf to its first keystroke.
fn sys_preadv2(fd: usize, iov_ptr: usize, iovcnt: usize, pos: usize, flags: usize, v2: bool) -> isize {
    rw_vectored(fd, iov_ptr, iovcnt, pos, flags, v2, false)
}

/// pwritev(2) / pwritev2(2); see `sys_preadv2`. RWF_DSYNC/RWF_SYNC make the
/// written data durable before returning (fdatasync/fsync of the file).
fn sys_pwritev2(fd: usize, iov_ptr: usize, iovcnt: usize, pos: usize, flags: usize, v2: bool) -> isize {
    rw_vectored(fd, iov_ptr, iovcnt, pos, flags, v2, true)
}

fn rw_vectored(fd: usize, iov_ptr: usize, iovcnt: usize, pos: usize, flags: usize,
               v2: bool, write: bool) -> isize {
    let pos = pos as isize;
    if v2 && flags & !RWF_SUPPORTED != 0 { return -95; } // EOPNOTSUPP
    let e = check_iov(iov_ptr, iovcnt);
    if e != 0 { return e; }
    if pos < -1 || (pos == -1 && !v2) { return -22; } // EINVAL
    let pid = current_pid();
    let nowait = flags & RWF_NOWAIT != 0;
    let ap: u64 = if flags & RWF_NOAPPEND != 0 { 1 } else if flags & RWF_APPEND != 0 { 2 } else { 0 };
    if nowait && !is_sock_handle(fd) {
        match vfs::fd_rwf_nowait_capable(pid, fd) {
            None => return -9,          // EBADF
            Some(false) => return -95,  // EOPNOTSUPP
            Some(true) => {}
        }
    }
    if iovcnt == 0 { return 0; }
    let mut total: isize = 0;
    if pos == -1 && !nowait {
        // The descriptor's own position, blocking as the descriptor says.
        total = if write { sys_writev(fd, iov_ptr, iovcnt) } else { sys_readv(fd, iov_ptr, iovcnt) };
        if total < 0 { return total; }
    } else {
        for i in 0..iovcnt {
            let (base, len) = iov_at(iov_ptr, i);
            if len == 0 { continue; }
            let bad = !validate_user_buf(base, len) || (!write && !user_buf_writable(base, len));
            if bad { return if total > 0 { total } else { -14 }; }
            if write { prefault_user_ro(base, len); } else { prefault_user(base, len); }
            let n = if pos == -1 {
                rw_nowait(fd, base, len, write)
            } else {
                // A positional transfer never waits on readiness, only on the
                // disk, which RWF_NOWAIT tolerates: it is a promise not to
                // block on locks or data that is not there yet, not one of
                // zero I/O.
                let at = (pos as usize).saturating_add(total as usize);
                if write { pwrite_at(pid, fd, base, len, at, ap) } else { pread_at(pid, fd, base, len, at) }
            };
            if n < 0 { return if total > 0 { total } else { n }; }
            total = total.saturating_add(n);
            if (n as usize) < len { break; } // a short transfer ends the vector
        }
    }
    if write && total > 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        let kind = if flags & RWF_SYNC != 0 { vfs::FSYNC_KIND_FILE } else { vfs::FSYNC_KIND_DATA };
        let r = sys_fsync(fd, kind);
        // A pipe or socket has nothing to make durable: fsync answers EINVAL
        // there, and the write itself already completed.
        if r < 0 && r != -22 { return r; }
    }
    total
}

/// fsync(fd) / fdatasync(fd) / syncfs(fd) — flush the filesystem behind `fd`.
///
/// These used to return 0 unconditionally, which was worse than ENOSYS: f2fs
/// keeps a write-back block cache and only checkpoints every 16 dirty
/// operations, so a successful fsync routinely left the data in RAM. Callers
/// that fsync precisely because they cannot afford to lose the write were
/// being told the opposite of the truth.
///
/// `kind` (`vfs::FSYNC_KIND_*`) goes to the filesystem: fsync and fdatasync
/// commit the file (f2fs skips the checkpoint when the file has nothing
/// uncommitted and batches concurrent ones), syncfs commits the whole mount.
/// fdatasync is still treated like fsync: the checkpoint is all-or-nothing,
/// so collapsing them is conservative, never wrong.
fn sys_fsync(fd: usize, kind: u64) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_FSYNC, &[fd as u64, kind]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// sync() — flush every mounted filesystem. Cannot fail.
fn sys_sync() -> isize {
    sync_all();
    0
}

/// Body of sync(2), also the first half of the reboot(2) device shutdown
/// (crate::power).
pub(crate) fn sync_all() {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_SYNC, &[]);
    let _ = vfs::handle(&msg, pid);
    crate::klog::flush_now();
}

/// sys_ftruncate(fd, length) — set tmpfs file size.
fn sys_ftruncate(fd: usize, length: usize) -> isize {
    let pid = current_pid();
    // RLIMIT_FSIZE: growing a file past it is SIGXFSZ + EFBIG (Linux
    // inode_newsize_ok). Only regular files have a size to limit.
    if (length as isize) >= 0 && fd_is_regular_file(pid, fd) {
        let lim = sched::rlimit_of(pid, sched::task::RLIMIT_FSIZE);
        if lim != sched::task::RLIM_INFINITY && length as u64 > lim {
            let _ = sched::fsize_check(pid, lim, u64::MAX, 0);
            return -27;
        }
    }
    // A mapped f2fs file (tmpfs is handled inside the VFS): once its end
    // moves down, every mapping loses the pages past it, so the next touch
    // re-faults and gets SIGBUS, as on Linux. Until 2026-10-19 the mapper
    // kept the frame the page cache had just dropped (a private copy that
    // no longer was the file). Growing leaves nothing to drop: pages past
    // the old end were never resident.
    let mapped = match vfs::vfs_get_node_kind(pid, fd) {
        Some(vfs::VnodeKind::MountedFile { port, file_id }) =>
            f2fs_server::inode_by_port(port, file_id as u64).and_then(|ino| mmap_cap_get(port, ino)),
        _ => None,
    };
    let msg = make_vfs_msg(vfs::VFS_FTRUNCATE, &[fd as u64, length as u64]);
    let r = vfs_reply_val(&vfs::handle(&msg, pid));
    if let Some(cap) = mapped {
        if r == 0 {
            let from = (length as u64).saturating_add(4095) & !4095;
            sched::zap_backing_all(cap, 0, from, u64::MAX);
        }
        mmap_file_ref(cap, false);
    }
    r
}

/// Is `fd` a regular file (f2fs or tmpfs)? Size limits apply to nothing else.
fn fd_is_regular_file(pid: u32, fd: usize) -> bool {
    if fd >= net_server::SOCK_FD_BASE { return false; }
    matches!(vfs::vfs_get_node_kind(pid, fd),
             Some(vfs::VnodeKind::MountedFile { .. }) | Some(vfs::VnodeKind::TmpFile { .. }))
}

/// fallocate(fd, mode, offset, len) — see vfs::handle_fallocate for the
/// supported modes. This used to answer 0 without doing anything, so
/// posix_fallocate reported success and left the file at its old size.
fn sys_fallocate(fd: usize, mode: usize, offset: usize, len: usize) -> isize {
    // Sockets and epoll instances live outside the VFS fd table.
    if fd >= net_server::SOCK_FD_BASE { return -29; } // ESPIPE
    let pid = current_pid();
    // RLIMIT_FSIZE, as for ftruncate (Linux checks offset + len whatever
    // the mode).
    if (offset as isize) >= 0 && (len as isize) > 0 && fd_is_regular_file(pid, fd) {
        let lim = sched::rlimit_of(pid, sched::task::RLIMIT_FSIZE);
        if lim != sched::task::RLIM_INFINITY && (offset as u64).saturating_add(len as u64) > lim {
            let _ = sched::fsize_check(pid, lim, u64::MAX, 0);
            return -27;
        }
    }
    let msg = make_vfs_msg(vfs::VFS_FALLOCATE, &[fd as u64, mode as u64, offset as u64, len as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// sys_times(buf_ptr) — the process's and its waited-for children's user and
/// system CPU times, in clock ticks; returns the ticks since boot.
fn sys_times(buf_ptr: usize) -> isize {
    // struct tms { tms_utime, tms_stime, tms_cutime, tms_cstime } all u64 = 32
    // bytes, in clock ticks (1/100 s, `sysconf(_SC_CLK_TCK)`): the caller's
    // process and the children it has waited for (`sched::ChildAcct`).
    if buf_ptr != 0 && validate_user_buf(buf_ptr, 32) {
        let tgid = sched::current_tgid();
        let (cpu, sys) = sched::process_cpu_sys_ns(tgid);
        let ch = sched::process_child_acct(tgid);
        let v = [cpu.saturating_sub(sys), sys, ch.cpu_ns.saturating_sub(ch.sys_ns), ch.sys_ns]
            .map(|ns| ns / 10_000_000);
        let mut out = [0u8; 32];
        for (i, x) in v.iter().enumerate() { out[i * 8..i * 8 + 8].copy_from_slice(&x.to_ne_bytes()); }
        prefault_user(buf_ptr, 32);
        copy_to_user_bytes(buf_ptr, &out);
    }
    ticks() as isize
}

/// sys_poll(fds_ptr, nfds, timeout_ms) — old-style poll syscall.
#[cfg(not(target_arch = "aarch64"))]
fn sys_poll(fds_ptr: usize, nfds: usize, timeout_ms: isize) -> isize {
    // `poll(NULL, 0, ms)` is a portable sleep: nothing to probe, but the
    // timeout still has to elapse (it used to return 0 at once).
    let sz = nfds.saturating_mul(8);
    if nfds != 0 && !user_readable(fds_ptr, sz) { return -14; }

    // The low 32 bits are the C `int`; a caller that passed -1 through a
    // zero-extending register still means "forever".
    let timeout_ms = timeout_ms as i32;
    let deadline = if timeout_ms < 0 { u64::MAX } else { deadline_after_ms(timeout_ms as u64) };
    poll_masked(fds_ptr, nfds, deadline, NO_SIGMASK, 0)
}

/// sys_ppoll(fds_ptr, nfds, timeout_ptr, sigmask_ptr) — wait for events on fd set.
///
/// Rechecks every struct pollfd against real per-fd readiness (see
/// `probe_fd_events`) in a cooperative retry loop until at least one is
/// ready or `timeout_ptr`'s `struct timespec` elapses (NULL = block
/// indefinitely, `{0,0}` = check once and return immediately). If all fds
/// report POLLNVAL (bad fd) or no events by the deadline, returns 0.
///
/// `sigmask_ptr` (with `sigsetsize` == 8) is installed for the wait and the
/// caller's mask restored on return — see `masked_timed_wait` — and the time
/// not slept is written back into `*timeout_ptr` (Linux `poll_select_finish`).
/// The sigmask used to be ignored, which broke the race-free
/// block-then-ppoll(unblocking mask) pattern: the signal stayed blocked and
/// the wait ran to its timeout.
fn sys_ppoll(fds_ptr: usize, nfds: usize, timeout_ptr: usize, sigmask_ptr: usize, sigsetsize: usize) -> isize {
    let sz = nfds.saturating_mul(8);
    if nfds != 0 && !user_readable(fds_ptr, sz) { return -14; }

    let (deadline, wb) = if timeout_ptr == 0 {
        (u64::MAX, 0)
    } else {
        match read_user_timespec(timeout_ptr) {
            // A zero timeout is never written back (Linux: "no update for
            // zero timeout").
            Ok(ns) => (deadline_after_ns(ns), if ns == 0 { 0 } else { timeout_ptr as u64 }),
            Err(e) => return e,
        }
    };
    let mask = if sigmask_ptr == 0 {
        NO_SIGMASK
    } else {
        match read_user_sigmask(sigmask_ptr, sigsetsize) { Ok(m) => m, Err(e) => return e }
    };
    poll_masked(fds_ptr, nfds, deadline, mask, wb)
}

/// poll/ppoll under `mask` (NO_SIGMASK = none) with timeout write-back `wb`;
/// also the `restart_syscall` resume of one.
fn poll_masked(fds_ptr: usize, nfds: usize, deadline: u64, mask: u64, wb: u64) -> isize {
    masked_timed_wait(mask, wb, deadline, || poll_wait(fds_ptr, nfds, deadline, mask, wb))
}

/// Read a ppoll/pselect6 sigmask argument. `sigsetsize` must be 8 (Linux:
/// EINVAL otherwise). SIGKILL and SIGSTOP are silently dropped, as
/// `sigprocmask` does.
fn read_user_sigmask(ptr: usize, sigsetsize: usize) -> Result<u64, isize> {
    if sigsetsize != 8 { return Err(-22); }
    if !validate_user_buf(ptr, 8) { return Err(-14); }
    match ua::read_user::<u64>(ptr) { Some(m) => Ok(m & !SIG_UNBLOCKABLE), None => Err(-14) }
}

/// The shared ppoll/pselect6/select frame (Linux `set_user_sigmask` +
/// `poll_select_finish`):
///
/// * `mask` (unless NO_SIGMASK) replaces the signal mask for the wait. If the
///   wait was interrupted, the caller's mask is parked with
///   `stash_sigsuspend_mask`: the signal that ended it is delivered on the way
///   out under the TEMPORARY mask, and the handler frame's `uc_sigmask` puts
///   the old one back — the same mechanism as sigsuspend. With no handler (a
///   stop/continue: the call restarts through `restart_syscall`, which
///   reinstalls `mask`) the delivery pass restores it directly. Any other
///   result restores the caller's mask at once, so a signal only the
///   temporary mask unblocked stays pending (Linux
///   `restore_saved_sigmask_unless(ret == -ERESTARTNOHAND)`).
/// * `wb` (unless 0) receives the time left until `deadline`, whatever the
///   result — timeout ({0,0}), ready, or interrupted.
fn masked_timed_wait(mask: u64, wb: u64, deadline: u64, body: impl FnOnce() -> isize) -> isize {
    let old = if mask != NO_SIGMASK { Some(sched::replace_signal_mask(mask)) } else { None };
    let r = body();
    if let Some(old) = old {
        if r == ERESTART_RESTARTBLOCK || r == -4 {
            sched::stash_sigsuspend_mask(old);
        } else {
            sched::replace_signal_mask(old);
        }
    }
    if wb != 0 { write_back_timeout(wb, deadline); }
    r
}

/// Write the time left until `deadline` into the caller's select/pselect6/
/// ppoll timeout (`wb`: pointer | WB_TIMEVAL for a `struct timeval`). A fault
/// is ignored, as Linux does.
fn write_back_timeout(wb: u64, deadline: u64) {
    let ptr = (wb & !WB_TIMEVAL) as usize;
    if !validate_user_buf(ptr, 16) { return; }
    let rem = deadline.saturating_sub(monotonic_ns());
    if wb & WB_TIMEVAL != 0 {
        let _ = ua::write_user::<[i64; 2]>(ptr, [(rem / 1_000_000_000) as i64, ((rem % 1_000_000_000) / 1_000) as i64]);
    } else {
        let _ = write_user_timespec(ptr, rem);
    }
}

/// Bit `fd` of the user fd_set at `set` (false when unreadable).
#[inline]
fn user_fdset_bit(set: usize, fd: usize) -> bool {
    (ur::<u8>(set + fd / 8) >> (fd % 8)) & 1 != 0
}

/// Read a `T` from user memory, `T::default()` when the address faults:
/// for loops that already EFAULT-checked their buffer up front and only
/// must not crash on a sibling thread's racing munmap.
#[inline]
fn ur<T: Copy + Default>(p: usize) -> T { ua::read_user::<T>(p).unwrap_or_default() }

/// Write a `T` to user memory; a fault is ignored (see [`ur`]).
#[inline]
fn uw<T: Copy>(p: usize, v: T) { let _ = ua::write_user::<T>(p, v); }

/// Whether all of `[ptr, ptr+len)` is readable user memory (EFAULT check
/// before a loop that works through the buffer in place).
fn user_readable(ptr: usize, len: usize) -> bool {
    if !validate_user_buf(ptr, len) { return false; }
    let mut tmp = [0u8; 256];
    let mut off = 0;
    while off < len {
        let n = (len - off).min(tmp.len());
        if unsafe { ua::copy_from_user(tmp.as_mut_ptr(), ptr + off, n) } != 0 { return false; }
        off += n;
    }
    true
}

/// The poll/ppoll loop proper, on an absolute `deadline` (`u64::MAX` = none);
/// also what `restart_syscall` resumes. Interrupted by a deliverable signal it
/// is EINTR when a handler runs and otherwise restarts on the SAME deadline
/// (Linux's `do_restart_poll`); it used to be EINTR even for a stop/continue.
/// `mask`/`wb` only travel into the restart block (see `poll_masked`).
///
/// With nothing ready, a deliverable signal wins over an expired deadline, as
/// in Linux's `do_poll` — so a zero-timeout ppoll whose mask unblocks a
/// pending signal reports EINTR and runs the handler.
fn poll_wait(fds_ptr: usize, nfds: usize, deadline: u64, mask: u64, wb: u64) -> isize {
    use core::sync::atomic::Ordering::Relaxed;
    let pid = current_pid();
    let infinite = deadline == u64::MAX;
    sched::wakestat::PO_CALLS.fetch_add(1, Relaxed);
    if gd_stats() || !POLL_READY.load(Relaxed) {
        return poll_wait_legacy(pid, fds_ptr, nfds, deadline, mask, wb);
    }
    // ---- one pass ---- (a zero timeout, or anything already ready, ends
    // here: no tags, no listener, no RUN_QUEUE).
    let gen0 = poll_gen();
    // A small set probes tagged here and reads the wake epoch first: if no
    // poll-channel wake ran by the time the listener is installed, these
    // tags are the park's, and the second full pass is skipped
    // (`poll_wait_listen`).
    let epoch0 = sched::poll_wake_epoch();
    let ctl0 = epoll_ctl_gen();
    let retag0 = sched::poll_retag_gen();
    let mut tags = [sched::PollTag::NONE; POLL_PRE_TAGS];
    let pre = nfds <= POLL_PRE_TAGS;
    let mut nready = 0isize;
    let mut named = false;
    for i in 0..nfds {
        let (ready, fd) = poll_probe_entry(pid, fds_ptr, i, if pre { Some(&mut tags[i]) } else { None });
        if ready { nready += 1; }
        named |= fd >= 0;
    }
    if nready > 0 { return nready; }
    if interrupted() {
        sched::set_restart_block([RB_POLL, fds_ptr as u64, nfds as u64, deadline, mask, wb, 0, 0]);
        return ERESTART_RESTARTBLOCK;
    }
    if !infinite && monotonic_ns() >= deadline { return 0; }
    if !named {
        // A timed sleep (`poll(NULL, 0, ms)`, every fd negative): the old
        // loop's `POLL_MASK_TIMED_ONLY` park.
        return poll_wait_legacy(pid, fds_ptr, nfds, deadline, mask, wb);
    }
    let pre = if pre { Some(PollPre { tags: &tags[..nfds], epoch: epoch0, ctl: ctl0, retag: retag0 }) } else { None };
    poll_wait_listen(pid, fds_ptr, nfds, deadline, mask, wb, gen0, pre)
}
/// Sets up to this size are probed tagged by `poll_wait`'s first pass.
const POLL_PRE_TAGS: usize = 16;
/// `poll_wait`'s tagged first pass and the generations read before it.
struct PollPre<'a> { tags: &'a [sched::PollTag], epoch: u64, ctl: u64, retag: u64 }

/// `poll.ready=0` on the kernel command line: the legacy park loop.
pub static POLL_READY: AtomicBool = AtomicBool::new(true);

/// Probe pollfd `i` of the array at `fds_ptr` and write its revents. With
/// `tag`, the probe's wake tag is stored there. Returns (counts as ready, fd).
/// A negative fd reads as not ready and contributes nothing.
fn poll_probe_entry(pid: u32, fds_ptr: usize, i: usize, tag: Option<&mut sched::PollTag>) -> (bool, i32) {
    const POLLNVAL: i16 = 0x0020;
    let pfd = fds_ptr + i * 8;
    let fd     = ur::<i32>(pfd      );
    let events = ur::<i16>(pfd + 4);
    let revents = if fd < 0 {
        if let Some(t) = tag { *t = sched::PollTag::NONE; }
        0
    } else if vfs::is_handle(fd as usize) {
        if let Some(t) = tag { *t = sched::PollTag::NONE; }
        POLLNVAL
    } else {
        sched::wakestat::PO_PROBES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        match tag {
            Some(t) => {
                let (ev, _seq, tg) = probe_fd_events_seq(pid, fd as usize, events as u16 as u32);
                *t = tg;
                ev as i16
            }
            None => probe_fd_events(pid, fd as usize, events as u16 as u32) as i16,
        }
    };
    uw::<i16>(pfd + 6, revents);
    (revents != 0, fd)
}

/// The park half of poll/ppoll on the epoll ready-list machinery (lane
/// epollabi). The old loop probed the whole pollfd array about three times
/// per sleep — the pass, `poll_fds_mask` for the tags, `poll_any_ready`
/// between prepare and commit — and once more after every wake, useful or
/// not. Now the thread LISTENS (`sched::poll_listen_begin`): every wake that
/// matches its filter lands in its ring whether it is parked or still
/// probing, so after the first tagged pass a wake re-probes only the entries
/// whose tag it names (`ep_hit`, as epoll does), and `poll_listen_park`
/// parks only with an empty ring (no wake can fall between the look and the
/// sleep). A full pass runs under the record-everything filter, so a wake
/// that lands while it probes is never filtered out; an entry whose tag
/// changed, an EPOLL_CTL_GEN move (only for a set holding an epoll fd), or a wake with no recorded reason starts
/// another full pass.
fn poll_wait_listen(pid: u32, fds_ptr: usize, nfds: usize, deadline: u64, mask: u64, wb: u64,
                    _gen0: u64, pre: Option<PollPre>) -> isize {
    use core::sync::atomic::Ordering::Relaxed;
    use sched::wakestat as ws;
    let infinite = deadline == u64::MAX;
    // The generation the park re-checks. EPOLL_CTL_GEN only matters when the
    // set holds an epoll fd (`tag.nested`): an epoll_ctl elsewhere cannot
    // change what a plain fd answers, and every successful epoll_ctl in the
    // system bumps it (Firefox's IPC threads MOD constantly), which used to
    // cancel nearly every ppoll park and force a full re-probe. The retag
    // generation (a socket whose tag changed with its state) always counts.
    let mut nested = false;
    let mut ctl0 = 0u64;
    let mut retag0 = 0u64;
    let gen_now = |nested: bool, ctl0: u64, retag0: u64| -> bool {
        sched::poll_retag_gen() != retag0 || (nested && epoll_ctl_gen() != ctl0)
    };
    let park_deadline = if infinite { u64::MAX } else { deadline };
    let mut tags = alloc::vec![sched::PollTag::NONE; nfds];
    let mut full = true;
    let mut ring = sched::WakeRing::EMPTY;
    let mut lid = match pre {
        // The first pass's tags, installed directly. No poll-channel wake
        // since the epoch read before that pass (and no retag, nor an
        // epoll_ctl for a set holding an epoll fd): nothing the pass saw
        // can have changed unannounced, and every later wake lands in the
        // ring. Otherwise fall back to the record-everything full pass.
        Some(p) => {
            tags.copy_from_slice(p.tags);
            nested = tags.iter().any(|t| t.nested != 0);
            ctl0 = p.ctl;
            retag0 = p.retag;
            let mut set = sched::PollSet::new();
            for t in tags.iter() { if t.bits != 0 { poll_set_add(&mut set, pid, t, 0); } }
            let (id, moved) = sched::poll_listen_begin_since(&set, p.epoch);
            if !moved && !gen_now(nested, ctl0, retag0) {
                ws::PO_PRESKIP.fetch_add(1, Relaxed);
                full = false;
            } else {
                let mut all = sched::PollSet::new();
                all.add_bits(sched::POLL_TAG_ALL);
                let _ = sched::poll_listen_update(id, &all);
            }
            id
        }
        None => sched::poll_listen_begin(None),
    };
    let mut woke = false;
    let r = loop {
        let mut nready = 0isize;
        if full {
            ctl0 = epoll_ctl_gen();
            retag0 = sched::poll_retag_gen();
            nested = false;
            for i in 0..nfds {
                if poll_probe_entry(pid, fds_ptr, i, Some(&mut tags[i])).0 { nready += 1; }
                nested |= tags[i].nested != 0;
            }
            if nready > 0 { break nready; }
            let mut set = sched::PollSet::new();
            for t in tags.iter() { if t.bits != 0 { poll_set_add(&mut set, pid, t, 0); } }
            match sched::poll_listen_update(lid, &set) {
                Some(r) => { full = false; ring = r; }
                None => { lid = sched::poll_listen_begin(None); continue; }
            }
            // Wakes that landed during the pass: look at what they name.
            if !ring.is_empty() { continue; }
        } else {
            let mut changed = false;
            for i in 0..nfds {
                let t = tags[i];
                if t.bits == 0 || !(ep_always(&t) || ep_hit(&t, &ring)) { continue; }
                let mut nt = t;
                if poll_probe_entry(pid, fds_ptr, i, Some(&mut nt)).0 { nready += 1; }
                if nt != t { changed = true; }
            }
            ring = sched::WakeRing::EMPTY;
            if nready > 0 { break nready; }
            if changed {
                // A new answer: probe everything again under the
                // record-everything filter, then install the new one.
                let mut all = sched::PollSet::new();
                all.add_bits(sched::POLL_TAG_ALL);
                if sched::poll_listen_update(lid, &all).is_none() { lid = sched::poll_listen_begin(None); }
                full = true;
                continue;
            }
        }
        if woke { ws::PO_USELESS.fetch_add(1, Relaxed); woke = false; }
        if interrupted() {
            sched::set_restart_block([RB_POLL, fds_ptr as u64, nfds as u64, deadline, mask, wb, 0, 0]);
            break ERESTART_RESTARTBLOCK;
        }
        if !infinite && monotonic_ns() >= deadline { break 0; }
        match sched::poll_listen_park(lid, park_deadline) {
            sched::ListenPark::Taken(r) => { ring = r; }
            sched::ListenPark::Off => { lid = sched::poll_listen_begin(None); full = true; }
            sched::ListenPark::Parked => {
                let gen_moved = gen_now(nested, ctl0, retag0);
                if gen_moved || interrupted() || (!infinite && monotonic_ns() >= deadline) {
                    sched::block_on_poll_cancel();
                } else {
                    ws::PO_PARKS.fetch_add(1, Relaxed);
                    sched::block_on_poll_commit();
                    woke = true;
                }
                // The ring the wake handed over (lane percpu4: no
                // RUN_QUEUE hold; later wakes wait in the ring for the
                // next park).
                let r = sched::poll_listen_woken(lid);
                if !r.is_empty() && !gen_moved {
                    ring = r;
                } else {
                    // No recorded reason (a signal, the deadline, an
                    // epoll_ctl / retag somewhere): look at everything.
                    let mut all = sched::PollSet::new();
                    all.add_bits(sched::POLL_TAG_ALL);
                    if sched::poll_listen_update(lid, &all).is_none() { lid = sched::poll_listen_begin(None); }
                    full = true;
                }
                // A cancelled park whose reason is the signal or the
                // deadline: the loop top answers (nothing ready -> EINTR/0)
                // after the pass.
            }
        }
    };
    sched::poll_listen_end();
    r
}

/// The original poll loop: every pass probes every entry, and a park
/// collects the tags (`poll_fds_mask`) and re-probes (`poll_any_ready`)
/// before it sleeps. Kept for `poll.ready=0`, the `gd` census, and timed
/// sleeps that name no fd.
fn poll_wait_legacy(pid: u32, fds_ptr: usize, nfds: usize, deadline: u64, mask: u64, wb: u64) -> isize {
    // struct pollfd { fd: i32, events: i16, revents: i16 } = 8 bytes.
    const POLLNVAL: i16 = 0x0020;
    let infinite = deadline == u64::MAX;
    if gd_stats() {
        gd_inc(K_PO_CALL);
        let now = monotonic_ns();
        if !infinite && deadline <= now { gd_inc(K_PO_ZERO); }
        else if !infinite && deadline - now <= 2_000_000 { gd_inc(K_PO_SHORT); }
    }
    let mut gd_woke = 0u64;

    loop {
        let gen0 = poll_gen();
        let mut nready = 0isize;
        for i in 0..nfds {
            let pfd = fds_ptr + i * 8;
            let fd     = ur::<i32>(pfd      );
            let events = ur::<i16>(pfd + 4);

            if fd < 0 {
                uw::<i16>(pfd + 6, 0);
                continue;
            }
            let revents = if vfs::is_handle(fd as usize) { POLLNVAL }
                else { probe_fd_events(pid, fd as usize, events as u16 as u32) as i16 };
            uw::<i16>(pfd + 6, revents);
            if revents != 0 {
                nready += 1;
                if gd_stats() { gd_fd(fd as usize, 1); }
            }
        }
        if gd_stats() {
            let timed_out = !infinite && monotonic_ns() >= deadline;
            gd_site(core::mem::take(&mut gd_woke), nready > 0 || timed_out || interrupted(), true);
            if nready > 0 { gd_inc(K_PO_EV); } else if timed_out { gd_inc(K_PO_TMO); }
        }
        if nready > 0 { return nready; }
        if interrupted() {
            sched::set_restart_block([RB_POLL, fds_ptr as u64, nfds as u64, deadline, mask, wb, 0, 0]);
            return ERESTART_RESTARTBLOCK;
        }
        if !infinite && monotonic_ns() >= deadline { return 0; }

        if gd_stats() { gd_inc(K_PO_PARK); }
        poll_block(infinite, deadline, poll_fds_mask(pid, fds_ptr, nfds), gen0, || poll_any_ready(pid, fds_ptr, nfds));
        if gd_stats() { gd_woke = sched::gdwake::take(pid); }
    }
}

/// sys_nanosleep(rqtp, rmtp) — park until the requested interval has elapsed
/// on `monotonic_ns`.
///
/// POSIX requires nanosleep to suspend for *at least* the requested interval.
/// The deadline is `now + request` in nanoseconds, compared against the same
/// clock: a 500 µs request armed 9.9 ms into a tick parks through the next
/// tick edge (0.1 ms away) and the one after it, rather than returning at the
/// first edge 400 µs short. Earlier versions counted ticks — a sub-tick request
/// was first truncated to 0 (Mesa's 160 µs ring backoffs became no-ops and its
/// 4096-iteration budget elapsed in 17 ms), then rounded up to one tick, which
/// still fired at the next edge: `sleep(200 ms)` measured 196.9 ms. The tick
/// bounds how *late* the wake is (≤ 10 ms), never how early.
fn sys_nanosleep(rqtp_ptr: usize, rmtp_ptr: usize) -> isize {
    if rqtp_ptr == 0 { return 0; }
    let ns = match read_user_timespec(rqtp_ptr) { Ok(n) => n, Err(e) => return e };
    if ns == 0 { return 0; }
    sleep_until_ns(deadline_after_ns(ns), rmtp_ptr, true)
}

/// Shared sleep body for the relative and absolute paths: park on the poll
/// wait-channel until `monotonic_ns() >= deadline`. `rmtp_ptr` may be 0
/// (nothing to report back).
///
/// Interrupted by a deliverable signal it answers the way Linux does: EINTR
/// when a handler runs, otherwise (a stop and continue, a signal a sibling
/// thread took) a transparent restart — the relative form (`relative`)
/// through the restart block, so only the remainder of `deadline` is slept
/// (`hrtimer_nanosleep_restart`); the absolute form simply re-executes with
/// the same deadline (-ERESTARTNOHAND). It used to be a plain EINTR, which
/// Linux has not reported for a stopped nanosleep since 2.6.24.
fn sleep_until_ns(deadline: u64, rmtp_ptr: usize, relative: bool) -> isize {
    loop {
        if interrupted() {
            // Report the time still owed in `rmtp`. Leaving it untouched is
            // not a harmless omission: std::thread::sleep passes the *same*
            // timespec for both arguments and re-sleeps whatever it finds
            // there, so an unwritten rmtp makes every EINTR restart the full
            // duration from scratch. Callers may also pass rqtp == rmtp, so
            // the request was read out above before anything is written back.
            if rmtp_ptr != 0 && validate_user_buf(rmtp_ptr, 16) {
                let _ = write_user_timespec(rmtp_ptr, deadline.saturating_sub(monotonic_ns()));
            }
            if !relative { return ERESTARTNOHAND; }
            sched::set_restart_block([RB_NANOSLEEP, deadline, rmtp_ptr as u64, 0, 0, 0, 0, 0]);
            return ERESTART_RESTARTBLOCK;
        }
        if monotonic_ns() >= deadline { break; }
        // Block on the poll wait-channel until the sleep deadline instead of a
        // yield_now busy-poll: the old spin pinned a CPU for the whole sleep
        // (e.g. init's getty-loop `usleep(1s)` — pid 1 churning the scheduler
        // run-loop at 100 %+ CPU and starving every other task). The first
        // tick at or after `deadline` wakes us; a spurious early wake (another
        // waiter's nearer deadline) just re-checks the clock and re-blocks.
        sched::block_on_poll_prepare_masked(deadline, sched::POLL_MASK_TIMED_ONLY);
        if monotonic_ns() >= deadline || interrupted() {
            sched::block_on_poll_cancel();
        } else {
            sched::block_on_poll_commit();
        }
    }
    0
}

/// `TIMER_ABSTIME` — clock_nanosleep's absolute-deadline mode.
const TIMER_ABSTIME: usize = 1;

/// sys_clock_nanosleep(clockid, flags, rqtp, rmtp).
///
/// The relative form is `sys_nanosleep` on every clock (a relative interval
/// is the same length on the wall clock and the monotonic one). With
/// `TIMER_ABSTIME` the deadline is an instant on `clockid`: CLOCK_REALTIME is
/// translated onto the monotonic clock through the current offset, everything
/// else is read as monotonic. This used to dispatch straight to
/// `sys_nanosleep(a2, a3)`, discarding the flags, which turned "wait until
/// this absolute timestamp" into "sleep FOR that timestamp": a deadline a few
/// seconds into the future became a sleep of however long the machine has
/// been up plus a few seconds, and a wall-clock deadline became decades.
fn sys_clock_nanosleep(clkid: usize, flags: usize, rqtp_ptr: usize, rmtp_ptr: usize) -> isize {
    if flags & TIMER_ABSTIME == 0 {
        return sys_nanosleep(rqtp_ptr, rmtp_ptr);
    }
    if rqtp_ptr == 0 { return 0; }
    let ns = match read_user_timespec(rqtp_ptr) { Ok(n) => n, Err(e) => return e };
    let target = match clkid {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => sched::realtime_to_monotonic_ns(ns),
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => return -22, // EINVAL, as Linux
        _ => ns,
    };
    // A deadline already in the past is not an error; it is a completed sleep.
    if target <= monotonic_ns() { return 0; }
    // A wall-clock deadline is re-translated whenever the clock is stepped
    // while this thread sleeps (`sleep_until_realtime_ns`).
    if matches!(clkid, CLOCK_REALTIME | CLOCK_REALTIME_COARSE) {
        return sleep_until_realtime_ns(ns);
    }
    // Absolute sleeps have nothing to report in rmtp on EINTR (the caller
    // simply re-issues with the same deadline), so pass 0 for it.
    sleep_until_ns(target, 0, false)
}

/// clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME) until wall-clock instant
/// `rt_ns`: parks like [`sleep_until_ns`], registered as a wall-clock waiter
/// (`sched::block_on_poll_prepare_realtime`), so a clock step moves its
/// monotonic deadline and the instant is honoured however the clock moves.
/// Interrupted, it restarts with the same instant (-ERESTARTNOHAND).
fn sleep_until_realtime_ns(rt_ns: u64) -> isize {
    loop {
        if interrupted() { return ERESTARTNOHAND; }
        let deadline = sched::realtime_to_monotonic_ns(rt_ns);
        if monotonic_ns() >= deadline { return 0; }
        sched::block_on_poll_prepare_realtime(rt_ns, sched::POLL_MASK_TIMED_ONLY);
        if monotonic_ns() >= sched::realtime_to_monotonic_ns(rt_ns) || interrupted() {
            sched::block_on_poll_cancel();
        } else {
            sched::block_on_poll_commit();
        }
        sched::realtime_wait_done();
    }
}

/// sys_gettimeofday(tv_ptr, tz_ptr) — fill `struct timeval` with wall-clock time.
///
/// The same `realtime_ns` that `clock_gettime(CLOCK_REALTIME)` reports, so the
/// two can never disagree (they used to be the tick counter vs the counter:
/// up to 10 ms apart, drifting within every tick). `tz_ptr` is always written
/// as UTC (+0).
fn sys_gettimeofday(tv_ptr: usize, tz_ptr: usize) -> isize {
    // struct timeval { tv_sec: i64, tv_usec: i64 }
    if tv_ptr != 0 {
        if !validate_user_buf(tv_ptr, 16) { return -14; }
        let ns = realtime_ns();
        let tv_sec  = (ns / 1_000_000_000) as i64;
        let tv_usec = ((ns % 1_000_000_000) / 1_000) as i64;
        uw::<i64>(tv_ptr, tv_sec);
        uw::<i64>(tv_ptr + 8, tv_usec);
    }
    // struct timezone { tz_minuteswest: i32, tz_dsttime: i32 }
    if tz_ptr != 0 && validate_user_buf(tz_ptr, 8) {
        uw::<u64>(tz_ptr, 0);
    }
    0
}

/// sys_time(tloc) — CLOCK_REALTIME in whole seconds as a `time_t` (i64).
///
/// x86-64 only (AArch64 does not have syscall #201 for `time`).
#[cfg(not(target_arch = "aarch64"))]
fn sys_time(tloc: usize) -> isize {
    let t = (realtime_ns() / 1_000_000_000) as i64;
    if tloc != 0 && validate_user_buf(tloc, 8) {
        let _ = sched::uaccess::write_user::<i64>(tloc, t);
    }
    t as isize
}

/// sys_sysinfo(info_ptr) — fill Linux `struct sysinfo` (112 bytes).
fn sys_sysinfo(info_ptr_user: usize) -> isize {
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
    if !validate_user_buf(info_ptr_user, SYSINFO_SIZE) { return -14; }
    // Built in a kernel buffer, copied out once (PAN-safe).
    let mut kb = [0u64; (SYSINFO_SIZE).div_ceil(8)];
    let info_ptr = kb.as_mut_ptr() as usize;


    let ticks = ticks();
    let uptime = (ticks / 100) as i64;
    // Free memory estimate from buddy allocator.
    let free_pages = mm::buddy::free_pages();
    let total_pages = mm::buddy::total_pages();
    let page_size = mm::buddy::PAGE_SIZE as u64;
    // Read the run queue BEFORE any user memory is touched below: taking the
    // RUN_QUEUE lock around a write that can demand-page re-enters the
    // scheduler under its own lock.
    let procs = sched::process_count().min(u16::MAX as usize) as u16;

    unsafe {
        // uptime (i64 at offset 0)
        core::ptr::write_unaligned((info_ptr) as *mut i64, uptime);
        // loads[3] (u64 × 3 at offset 8): 1/5/15-minute load averages,
        // fixed point << 16 (SI_LOAD_SHIFT).
        let l = sched::loadavg();
        for (i, v) in l.iter().enumerate() {
            core::ptr::write_unaligned((info_ptr + 8 + i * 8) as *mut u64, sched::loadavg_si(*v));
        }
        // totalram (u64 at offset 32)
        core::ptr::write_unaligned((info_ptr + 32) as *mut u64, total_pages as u64 * page_size);
        // freeram (u64 at offset 40)
        core::ptr::write_unaligned((info_ptr + 40) as *mut u64, free_pages as u64 * page_size);
        // procs (u16 at offset 80)
        core::ptr::write_unaligned((info_ptr + 80) as *mut u16, procs);
        // mem_unit (u32 at offset 104) — 1 byte
        core::ptr::write_unaligned((info_ptr + 104) as *mut u32, 1u32);
    }
    let rv = 0;
    if unsafe { ua::copy_to_user(info_ptr_user, info_ptr as *const u8, SYSINFO_SIZE) } != 0 { return -14; }
    rv
}

// ── Signal delivery syscalls ───────────────────────────────────────────────────

fn sys_rt_sigaction(signum: usize, act_ptr: usize, oldact_ptr: usize) -> isize {
    if signum == 0 || signum > 64 { return -22; } // EINVAL
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
/// pid == -1 every process the caller may signal. sig == 0 is the existence
/// probe (no signal sent). Permission (Linux `check_kill_permission`) lives in
/// `sched::cred`: the sender's real or effective uid must match the target's
/// real or saved uid, or the sender is root, or the target is in the sender's
/// own thread group, or it is SIGCONT within the sender's session.
fn sys_kill(pid_raw: usize, sig_raw: usize) -> isize {
    let sig = sig_raw as u32;
    if sig > 64 { return -22; } // EINVAL
    let pid_i = pid_raw as u32 as i32; // pid_t travels sign-extended
    // SI_USER, carrying the *sender's* identity: the receiving handler's
    // `si_pid`/`si_uid` name who sent the signal, which is the whole content of
    // a kill(2) siginfo. The process id, not the thread id — a signal comes
    // from a process.
    let info = sched::SigInfo::user(sched::current_tgid(), sched::current_uid());
    if pid_i > 0 {
        // kill(2) is process-directed: route to a thread in the target group
        // that hasn't masked `sig`, not blindly its leader. A thread id names
        // its process, as on Linux.
        let k = match sched::pidns::from_cur(pid_i as u32) { Ok(k) => k, Err(e) => return e };
        return sched::cred::kill_process(sched::tgid_of(k), sig, info);
    }
    if pid_i == -1 { return sched::cred::kill_all_checked(sig, info); }
    let pgid = if pid_i == 0 { sched::current_pgid() } else {
        match sched::pidns::from_cur((-(pid_i as i64)) as u32) { Ok(k) => k, Err(e) => return e }
    };
    sched::cred::kill_pgrp_checked(pgid, sig, info)
}

/// getppid(2): the parent *process* of the calling process. A thread's own
/// `ppid` is the thread that created it, and a process forked from a
/// non-main thread records that thread; report the leader's parent, as a
/// thread group id.
fn sys_getppid() -> isize {
    let leader_ppid = sched::task_ppid(sched::current_tgid()).unwrap_or_else(|| current_ppid());
    let n = sched::pidns::cur();
    let p = sched::tgid_of(leader_ppid);
    // Outside the namespace (or the init's parent) -> 0, as Linux.
    (if n == 0 { p } else { sched::pidns::to_v(n, p) }) as isize
}

// ── Thread primitives (futex, TID address, TLS base) ──────────────────────────

/// get_robust_list(2): the robust-list head of thread `tid` (0 = self).
/// Another thread's only within the caller's group, or for root (Linux
/// asks ptrace_may_access).
fn sys_get_robust_list(tid: usize, head_ptr: usize, len_ptr: usize) -> isize {
    let Ok(tid) = sched::pidns::from_cur(tid as u32) else { return -3 };
    if tid != 0 && sched::tgid_of(tid) != sched::current_tgid() && sched::current_euid() != 0 {
        return -1; // EPERM
    }
    let Some(head) = sched::robust::get(tid) else { return -3 }; // ESRCH
    if !sched::uaccess::write_user::<usize>(head_ptr, head)
        || !sched::uaccess::write_user::<usize>(len_ptr, sched::robust::HEAD_LEN) {
        return -14;
    }
    0
}

fn sys_set_tid_address(tidptr: usize) -> isize {
    set_clear_child_tid(tidptr);
    sched::pidns::self_v(current_pid()) as isize
}

/// Timed FUTEX_WAIT with its relative timeout already turned into the
/// absolute `deadline`; also the `restart_syscall` resume of one. Interrupted
/// by a deliverable signal it stores `deadline` in the restart block, so a
/// restart (no handler ran) waits out only the remainder — Linux's
/// `futex_wait_restart`. It used to be ERESTARTNOHAND, which re-executed the
/// original call and re-read the full relative interval.
///
/// `realtime` is the original absolute CLOCK_REALTIME deadline of a
/// FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME wait: the restart re-translates
/// it onto the monotonic clock instead of reusing `deadline`.
fn futex_wait_timed(uaddr: usize, val: u32, private: bool, deadline: u64, realtime: Option<u64>) -> isize {
    // A wall-clock deadline follows clock steps while parked.
    let r = match realtime {
        Some(rt) => sched::futex_wait_intr_realtime(uaddr, val, rt, private),
        None => sched::futex_wait_intr(uaddr, val, Some(deadline), private),
    };
    if r != sched::FUTEX_INTERRUPTED { return r; }
    sched::set_restart_block([RB_FUTEX, uaddr as u64, val as u64, private as u64, deadline,
                              realtime.is_some() as u64, realtime.unwrap_or(0), 0]);
    ERESTART_RESTARTBLOCK
}

fn sys_futex(uaddr: usize, op: usize, val: usize, timeout_ptr: usize, uaddr2: usize, val3: usize) -> isize {
    // Strip FUTEX_PRIVATE_FLAG (128) and FUTEX_CLOCK_REALTIME (256). The
    // clock flag matters for FUTEX_WAIT_BITSET only: its absolute deadline is
    // then an instant on the wall clock, translated onto the monotonic clock
    // (FUTEX_WAIT's interval is relative on either clock, as on Linux).
    const FUTEX_PRIVATE_FLAG:   usize = 128;
    const FUTEX_CLOCK_REALTIME: usize = 256;
    const FUTEX_WAIT:           usize = 0;
    const FUTEX_WAIT_BITSET:    usize = 9;
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    // The private flag only skips the mapping lookup: sched::futex_key keys
    // a futex word in a private mapping by (thread group, address) whatever
    // the flag says, as Linux keys it by (mm, address). Mesa's futexes
    // (simple_mtx, util_queue fences) never pass the flag, and since mmap
    // reuses freed ranges per address space two processes running the same
    // code lay their heaps out at the same addresses (lane vmreuse2).
    let private = op & FUTEX_PRIVATE_FLAG != 0;
    match cmd {
        // FUTEX_WAIT and FUTEX_WAIT_BITSET: the bitset form is only ever
        // called with FUTEX_BITSET_MATCH_ANY by the libcs in this tree (musl,
        // relibc, Rust std), so the mask itself is not filtered on — the extra
        // uaddr2/val3 args aren't even consulted for it.
        //
        // The two differ in what the timeout MEANS, and that difference is
        // load-bearing: FUTEX_WAIT takes a RELATIVE interval, FUTEX_WAIT_BITSET
        // an ABSOLUTE deadline on CLOCK_MONOTONIC (or CLOCK_REALTIME with the
        // flag). musl's pthread code uses the relative form. Rust std's
        // `futex_wait` — under every `Condvar::wait_timeout`,
        // `thread::park_timeout`, `mpsc::recv_timeout` and `Mutex` spin fallback
        // in every Rust program — uses the bitset form with `now + timeout`.
        // Treating that as relative made every std timed wait last "uptime plus
        // the intended duration": harmless in the first seconds after boot,
        // and a wedge a few minutes in. async-io's driver thread backs off
        // with `park_timeout(≤10 ms)` while any thread is inside
        // `async_io::block_on` (zbus's executor thread is, permanently), and
        // at three minutes of uptime each of those became a three-minute
        // sleep — the session compositor started by greetd after the greeter
        // login sat in `zbus::Connection::session()` with its Hello reply
        // unread in the socket for minutes, while the same compositor started
        // a minute after boot came up fine. Same bug class
        // `sys_clock_nanosleep` fixed for TIMER_ABSTIME.
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            // FUTEX_WAIT: if *uaddr == val, block until woken.  The value
            // check happens inside futex_wait, between that task's
            // registration in FUTEX_TABLE and its commit to Blocked —
            // checking it out here would reopen the SMP lost-wake-up window
            // (another CPU could change the value and issue FUTEX_WAKE
            // between an early check and the waiter registration).  Note it
            // deliberately does NOT happen under the FUTEX_TABLE lock: the
            // read can take a demand-paging fault, and `handle_page_fault`
            // re-enters the scheduler.  See the sched::futex module docs.
            if !validate_user_ptr_aligned(uaddr, 4, 4) { return -14; }
            // timeout_ptr is a `struct timespec`; NULL means no timeout (block
            // indefinitely). Converted to an absolute `monotonic_ns` deadline.
            let mut realtime = None;
            let deadline = if timeout_ptr == 0 {
                None
            } else {
                let ns = match read_user_timespec(timeout_ptr) { Ok(n) => n, Err(e) => return e };
                if cmd == FUTEX_WAIT_BITSET {
                    // Absolute on the requested clock. A deadline already in
                    // the past is answered without parking, value check first
                    // (Linux reports EAGAIN over ETIMEDOUT when the word has
                    // moved).
                    let target = if op & FUTEX_CLOCK_REALTIME != 0 {
                        realtime = Some(ns);
                        sched::realtime_to_monotonic_ns(ns)
                    } else {
                        ns
                    };
                    if target <= monotonic_ns() {
                        // Already expired. No lock is held, so the read may
                        // fault and be serviced like any other user read.
                        let cur = ur::<u32>(uaddr);
                        return if cur != val as u32 { -11 } else { -110 }; // EAGAIN / ETIMEDOUT
                    }
                    Some(target)
                } else {
                    // Relative, exactly as sys_nanosleep converts it: a 3 ms
                    // wait is 3 ms, not "0 ticks, wake at the next edge".
                    Some(deadline_after_ns(ns))
                }
            };
            // Timed, either form: EINTR if a handler runs, else a restart on
            // the same absolute deadline (a CLOCK_REALTIME BITSET one is
            // re-translated from the wall clock on the restart).
            if let Some(d) = deadline { return futex_wait_timed(uaddr, val as u32, private, d, realtime); }
            let r = sched::futex_wait_intr(uaddr, val as u32, deadline, private);
            if r != sched::FUTEX_INTERRUPTED { return r; }
            // Released by a deliverable signal with no wake claimed
            // (futex_wait_intr decides that under the same FUTEX_TABLE hold
            // that frees the waiter, so a FUTEX_WAKE that claimed us is never
            // turned into a restart and lost). Linux (futex_wait):
            //   * untimed FUTEX_WAIT -> -ERESTARTSYS: SA_RESTART (or no
            //     handler run) re-executes it transparently, else EINTR;
            //   * timed FUTEX_WAIT -> -ERESTART_RESTARTBLOCK: EINTR whenever
            //     a handler runs, SA_RESTART or not; restarted only when none
            //     does, with the remainder of the original deadline
            //     (`futex_wait_timed`). Measured on Linux 7.0: timed + SA_RESTART
            //     handler -> EINTR at the signal; untimed -> restarted.
            //     Restarting timed waits under SA_RESTART would let a periodic
            //     signal postpone the timeout forever.
            // FUTEX_WAIT_BITSET answers the same way (Linux futex_wait does
            // not tell the two apart). It used to return a spurious 0 at the
            // signal: harmless for Rust std and relibc, which re-check, but
            // an SA_RESTART-less caller could not see the EINTR.
            ERESTARTSYS
        }
        1 => {
            // FUTEX_WAKE: wake up to `val` tasks sleeping on `uaddr`.
            sched::futex_wake_keyed(uaddr, val as u32, private) as isize
        }
        3 | 4 => {
            // FUTEX_REQUEUE = 3, FUTEX_CMP_REQUEUE = 4
            if !validate_user_ptr_aligned(uaddr, 4, 4) { return -14; }
            if !validate_user_ptr_aligned(uaddr2, 4, 4) { return -14; }
            if cmd == 4 {
                let current = match sched::uaccess::read_user::<u32>(uaddr) { Some(v) => v, None => return -14 };
                if current != val3 as u32 {
                    return -11; // EAGAIN
                }
            }
            sched::futex_requeue_keyed(uaddr, uaddr2, val as u32, timeout_ptr as u32, private)
        }
        // FUTEX_LOCK_PI = 6, FUTEX_UNLOCK_PI = 7, FUTEX_TRYLOCK_PI = 8,
        // FUTEX_LOCK_PI2 = 13: see futex_lock_pi.
        6 | 13 => {
            if !validate_user_ptr_aligned(uaddr, 4, 4) { return -14; }
            // LOCK_PI's timeout is an absolute CLOCK_REALTIME instant (the
            // flag is implied); LOCK_PI2's is CLOCK_MONOTONIC unless
            // FUTEX_CLOCK_REALTIME is passed.
            let deadline = if timeout_ptr == 0 {
                None
            } else {
                let ns = match read_user_timespec(timeout_ptr) { Ok(n) => n, Err(e) => return e };
                Some(if cmd == 6 || op & FUTEX_CLOCK_REALTIME != 0 {
                    sched::realtime_to_monotonic_ns(ns)
                } else {
                    ns
                })
            };
            futex_lock_pi(uaddr, private, deadline, false)
        }
        8 => {
            if !validate_user_ptr_aligned(uaddr, 4, 4) { return -14; }
            futex_lock_pi(uaddr, private, None, true)
        }
        7 => {
            if !validate_user_ptr_aligned(uaddr, 4, 4) { return -14; }
            futex_unlock_pi(uaddr, private)
        }
        _ => -38, // ENOSYS
    }
}

// ── PI futexes (FUTEX_LOCK_PI / UNLOCK_PI / TRYLOCK_PI) ──────────────────────
//
// The lock word is Linux's: owner TID in bits 0..29, FUTEX_OWNER_DIED (bit
// 30), FUTEX_WAITERS (bit 31). User space takes a free lock with
// cas(0 -> tid) and releases it with cas(tid -> 0); it enters the kernel only
// when that fails, i.e. when the lock is held (LOCK_PI) or FUTEX_WAITERS is
// set (UNLOCK_PI). musl uses them for every PTHREAD_PRIO_INHERIT mutex, and
// `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` first PROBES the call
// with FUTEX_LOCK_PI on a zero word: any error other than success makes the
// attribute call fail, and libpulse (`pa_mutex_new`) asserts on that failure
// (`r == 0 || r == ENOTSUP`) and aborts — Firefox's audio server thread
// died there the moment cubeb opened its PulseAudio backend.
//
// What is NOT implemented is the priority inheritance itself (no RT
// scheduling class to boost into). FUTEX_UNLOCK_PI hands the lock to the
// first queued locker, as Linux does: the word becomes its TID |
// FUTEX_WAITERS before it runs (sched::futex_unlock_pi), so a user-space cas
// cannot take it in between. Waiting reuses the ordinary futex queue
// (sched::futex), keyed and value-checked the same way: a locker sets
// FUTEX_WAITERS and parks expecting exactly that word, so an unlock (or any
// change) between the two turns into EAGAIN and a retry, never a lost
// wake-up. User memory is only touched with no kernel lock held.
// FUTEX_OWNER_DIED is set by the robust-list walk at the owner's death
// (sched::robust) and kept by the next locker, which reports EOWNERDEAD.
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

/// FUTEX_LOCK_PI / LOCK_PI2 (`try` = false) and FUTEX_TRYLOCK_PI (`try` =
/// true). Returns 0 with the word = our TID (| FUTEX_WAITERS when others are
/// still queued, | FUTEX_OWNER_DIED if it was set); EDEADLK if we already own
/// it; ESRCH if the owner TID names no task; EAGAIN (trylock) when held;
/// ETIMEDOUT at the deadline; a restartable interruption on a signal (musl
/// retries EINTR itself).
fn futex_lock_pi(uaddr: usize, private: bool, deadline: Option<u64>, try_only: bool) -> isize {
    let ktid = current_pid() as u32;
    // The tid written to the word is the caller's own view (1 for a pid-ns init).
    let tid = sched::pidns::self_v(ktid);
    // The word goes through the fault-tolerant uaccess helpers: a bad
    // pointer is EFAULT, not a kernel fault.
    let cas = |old: u32, new: u32| -> Result<bool, isize> {
        match ua::cmpxchg_user_u32(uaddr, old, new) { Some(s) => Ok(s == old), None => Err(-14) }
    };
    loop {
        let w = match ua::load_user_u32(uaddr) { Some(v) => v, None => return -14 };
        let owner = w & FUTEX_TID_MASK;
        if owner == 0 {
            // Free: take it. Keep FUTEX_WAITERS set while anyone else is
            // queued, so our unlock comes through the kernel and wakes them.
            let mut new = tid | (w & FUTEX_OWNER_DIED);
            if sched::futex_has_waiters(uaddr, private, ktid) { new |= FUTEX_WAITERS; }
            match cas(w, new) { Ok(true) => return 0, Ok(false) => {}, Err(e) => return e }
            continue;
        }
        if owner == tid { return -35; } // EDEADLK
        if try_only { return -11; }     // EWOULDBLOCK
        let owner_k = match sched::pidns::from_cur(owner) { Ok(k) => k, Err(e) => return e };
        if !sched::futex_task_exists(owner_k) { return -3; } // ESRCH
        // Held: make the owner's unlock come to the kernel, then park
        // expecting exactly that word. FUTEX_WAITERS goes in BEFORE any
        // timeout verdict: a woken locker that lost the free word to a
        // user-space cas and then gives up must still leave the bit behind,
        // or the new owner's user-space unlock would strand the lockers that
        // are still parked.
        let expected = w | FUTEX_WAITERS;
        if w & FUTEX_WAITERS == 0 {
            match cas(w, expected) { Ok(true) => {}, Ok(false) => continue, Err(e) => return e }
        }
        if let Some(d) = deadline {
            if monotonic_ns() >= d { return -110; } // ETIMEDOUT
        }
        match sched::futex_wait_pi(uaddr, expected, deadline, private) {
            // FUTEX_UNLOCK_PI handed us the lock: the word is ours already.
            sched::FUTEX_HANDOFF => return 0,
            // Woken by an unlock, or the word moved before we parked: retry.
            0 | -11 => continue,
            sched::FUTEX_INTERRUPTED => return ERESTARTSYS,
            -110 => return -110,
            r => return r,
        }
    }
}

/// FUTEX_UNLOCK_PI: EPERM unless the caller owns the word; otherwise hand it
/// to the first queued locker, or release it to 0 (FUTEX_OWNER_DIED is
/// dropped with the ownership). See sched::futex_unlock_pi.
fn futex_unlock_pi(uaddr: usize, private: bool) -> isize {
    sched::futex_unlock_pi(uaddr, private, sched::pidns::self_v(current_pid() as u32))
}

fn sys_arch_prctl(code: usize, addr: usize) -> isize {
    // ARCH_SET_FS = 0x1002, ARCH_GET_FS = 0x1003 (x86-64 only)
    #[cfg(target_arch = "x86_64")]
    {
        const ARCH_SET_GS: usize = 0x1001;
        const ARCH_SET_FS: usize = 0x1002;
        const ARCH_GET_FS: usize = 0x1003;
        const ARCH_GET_GS: usize = 0x1004;
        match code {
            // A per-thread user GS base, restored on every return to user
            // mode. wasm2c "segue" sandboxes (Firefox's RLBox libraries) put
            // their memory base here and abort when this fails.
            ARCH_SET_GS => {
                if addr >= 0x0000_8000_0000_0000 { return -1; } // EPERM: not a user address
                sched::set_user_gs_base(addr as u64);
                0
            }
            ARCH_GET_GS => {
                if !validate_user_ptr_aligned(addr, 8, 8) { return -14; }
                let base = sched::get_user_gs_base();
                unsafe { let _ = sched::uaccess::write_user::<u64>(addr, base); }
                0
            }
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
                unsafe { let _ = sched::uaccess::write_user::<u64>(addr, base); }
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
    // PROT_EXEC on a mapping of a file from a noexec mount: EPERM.
    if prot & 4 != 0
        && with_current_address_space_mut(|as_| as_.range_noexec_mount(addr, len)) == Some(true) {
        return -1;
    }
    let ok = with_current_address_space_mut(|as_| as_.mprotect(addr, len, prot as u32));
    match ok {
        Some(true)  =>  0,
        Some(false) => -22, // EINVAL
        None        => -1,
    }
}

/// `madvise(2)`. MADV_DONTNEED really drops the pages (see
/// `AddressSpace::discard_range`); every other advice is a hint this kernel
/// may ignore, MADV_FREE included (its contract allows the old data to stay).
fn sys_madvise(addr: usize, len: usize, advice: usize) -> isize {
    const MADV_DONTNEED: usize = 4;
    if addr & 0xFFF != 0 { return -22; } // EINVAL
    if advice != MADV_DONTNEED || len == 0 { return 0; }
    match with_current_address_space_mut(|as_| as_.discard_range(addr, len)) {
        Some(true)  => 0,
        Some(false) => -12, // ENOMEM: part of the range is not mapped
        None        => -22,
    }
}

/// `brk(2)` has no errno. Failure is "the returned break is not the one you
/// asked for", which musl detects by comparing the two and turns into a NULL
/// from `malloc` — so a heap that cannot grow is invisible on the way out.
/// `DBG_ENOMEM` makes it visible.
fn sys_brk(new_end: usize) -> isize {
    // RLIMIT_DATA / RLIMIT_AS: a refused growth leaves the break where it
    // is (Linux's brk answers the old break, never an errno).
    if new_end != 0 {
        if let Some((start, cur)) = with_current_address_space(|a|
            (a.heap_start, if a.heap_end != 0 { a.heap_end } else { a.heap_start }))
        {
            if start != 0 && new_end > cur {
                let grow = new_end - cur;
                let lim = sched::current_rlimit(sched::task::RLIMIT_DATA);
                let heap_over = lim != sched::task::RLIM_INFINITY
                    && (new_end - start) as u64 > lim;
                if heap_over || !vm_limits_admit(grow, false) { return cur as isize; }
            }
        }
    }
    let r = with_current_address_space_mut(|as_| as_.brk(new_end))
        .unwrap_or_else(|| enomem_site("brk/no-address-space"));
    if new_end != 0 && r >= 0 && (r as usize) < new_end {
        let _ = enomem_map_site(("brk/refused"));
    }
    r
}

/// POSIX `mincore(addr, length, vec)` — report which pages of `[addr, addr+length)`
/// are resident in core.
///
/// LeandrOS has no swap, so every *mapped* page is resident. The load-bearing
/// part of the contract is the error result, not the bitmap: callers such as
/// Mesa's `_eglPointerIsDereferenceable` treat a non-negative return purely as
/// "this address is mapped, so the pointer is real." The previous stub always
/// returned success, which made that probe wrongly accept `(void*)3` and drove
/// cosmic-panel's `wl_egl_window.version` into `wl_proxy_create_wrapper((wl_surface*)3)`
/// (FAR=0x1B). We must return -ENOMEM whenever the range covers an unmapped page.
///
/// Semantics (matching Linux):
///   - `addr` not page-aligned            → -EINVAL
///   - `length == 0`                       → 0 (nothing to report)
///   - range wraps the address space       → -ENOMEM
///   - any page in the range has no VMA    → -ENOMEM
///   - `vec` unwritable                    → -EFAULT
///   - otherwise: write one byte per page (bit 0 = resident) and return 0.
fn sys_mincore(addr: usize, length: usize, vec: usize) -> isize {
    let page = mm::buddy::PAGE_SIZE;
    if addr % page != 0 { return -22; } // EINVAL — addr must be page-aligned
    if length == 0 { return 0; }
    // Round length up to whole pages; reject a range that wraps the address space.
    let pages = (length - 1) / page + 1;
    let end = match pages.checked_mul(page).and_then(|b| addr.checked_add(b)) {
        Some(e) => e,
        None => return -12, // ENOMEM
    };

    // One address-space hold per CHUNK pages, with an interrupt window
    // between holds: a mincore over a multi-GiB reservation used to walk the
    // whole range twice (find() per page, then the PTE walk) in one hold with
    // IRQs masked, and every sibling CPU spun on the busy flag meanwhile.
    // Linux also fills `vec` progressively and fails with ENOMEM at the first
    // hole, leaving the earlier bytes written.
    const CHUNK: usize = 256;
    let mut done = 0usize;
    while done < pages {
        let n = core::cmp::min(CHUNK, pages - done);
        let r = with_current_address_space_mut(|as_| {
            let mut buf = [0u8; CHUNK];
            let mut vma_end = 0usize;
            for i in 0..n {
                let pva = addr + (done + i) * page;
                if pva >= vma_end {
                    match as_.find(pva) {
                        Some(v) => vma_end = v.end,
                        None => {
                            // Hole: keep the bytes already computed in this hold.
                            if i > 0 { let _ = as_.write_user_buf(vec + done, &buf[..i]); }
                            return -12isize;
                        }
                    }
                }
                buf[i] = if as_.virt_to_phys(pva).is_some() { 1 } else { 0 };
            }
            if !as_.write_user_buf(vec + done, &buf[..n]) { return -14isize; } // EFAULT
            0
        }).unwrap_or(-12); // no current address space
        if r != 0 { return r; }
        done += n;
        if done < pages { irq_window(); }
    }
    let _ = end;
    0
}

// ── execve ────────────────────────────────────────────────────────────────────

// ── execve string-building infrastructure ────────────────────────────────────

/// Linux's argument limits. All of argv + envp (strings and their pointers)
/// may take a quarter of the stack, as with an 8 MiB RLIMIT_STACK on Linux
/// (`ARG_MAX` = 2 MiB there); one string may be at most MAX_ARG_STRLEN.
/// Anything larger is E2BIG, never a silent truncation: until 2026-10-13
/// argv and envp were each capped at 64 entries and 8 KiB, and the excess
/// was dropped without an error, so `xargs sha256sum` hashed 60 of 200 files
/// and exited 0, and cargo's ~80-variable build-script environment lost
/// most of its CARGO_* variables.
const EXEC_ARG_MAX:   usize = USER_STACK_SIZE / 4;
const MAX_ARG_STRLEN: usize = 32 * 4096;

/// argv or envp as collected during execve, before the address space is
/// replaced. Lives in a static (EXEC_ARGV/EXEC_ENVP) because
/// replace_address_space never returns, so a local's heap would never be
/// freed; `release` trims what a huge exec grew it to.
struct ExecStrBuf {
    data:    Vec<u8>,    // the strings, each NUL-terminated
    end:     usize,      // == data.len()
    offsets: Vec<usize>, // start offset of each string in data[]
    lengths: Vec<usize>, // byte length (excl. NUL) of each string
    count:   usize,      // == offsets.len()
}

impl ExecStrBuf {
    const fn new() -> Self {
        Self { data: Vec::new(), end: 0, offsets: Vec::new(), lengths: Vec::new(), count: 0 }
    }

    /// Bytes this buffer will take on the new stack: strings plus pointers.
    fn footprint(&self) -> usize { self.end + self.count * core::mem::size_of::<usize>() }

    fn commit(&mut self, start: usize) {
        self.offsets.push(start);
        self.lengths.push(self.data.len() - start - 1);
        self.count += 1;
        self.end = self.data.len();
    }

    /// Read one null-terminated C string from user-space `ptr` into the buffer.
    /// `budget` is what argv + envp may still take (see EXEC_ARG_MAX).
    /// Err(-7) E2BIG when it does not fit, Err(-14) EFAULT on a bad pointer.
    ///
    /// The user pointer is NEVER raw-dereferenced: each page-bounded chunk is
    /// first faulted in (`prefault_user`, backing a demand-paged .rodata/.data
    /// argv literal) and then copied through the address space's fault-checked
    /// `read_user_buf`.  A bad/unmapped pointer (or one whose backing page can't
    /// be faulted) fails safely here instead of taking a kernel-mode data abort
    /// at EL1 — which, if unserviceable, would hang the machine.
    fn push_cstr(&mut self, ptr: usize, budget: usize) -> Result<(), isize> {
        if ptr == 0 { return Err(-14); }
        let start = self.data.len();
        let page = mm::buddy::PAGE_SIZE;
        let mut buf = [0u8; 256];
        let ptr_cost = core::mem::size_of::<usize>();
        loop {
            let len = self.data.len() - start;
            if len >= MAX_ARG_STRLEN || len + ptr_cost + 1 > budget {
                self.data.truncate(start);
                return Err(-7); // E2BIG
            }
            let read_from = ptr + len;
            let to_page = page - (read_from % page);
            let chunk = core::cmp::min(to_page, buf.len());
            prefault_user_ro(read_from, chunk);
            let ok = with_current_address_space(|as_| {
                as_.read_user_buf(read_from, &mut buf[..chunk])
            }).unwrap_or(false);
            if !ok { self.data.truncate(start); return Err(-14); }
            if let Some(z) = buf[..chunk].iter().position(|&b| b == 0) {
                self.data.extend_from_slice(&buf[..=z]);
                if self.data.len() - start + ptr_cost > budget {
                    self.data.truncate(start);
                    return Err(-7);
                }
                self.commit(start);
                return Ok(());
            }
            self.data.extend_from_slice(&buf[..chunk]);
        }
    }

    /// Append one string that already lives in kernel memory — the
    /// interpreter, its optional argument and the script path a `#!` line
    /// puts in front of the caller's argv. Returns false when it does not fit.
    fn push_bytes(&mut self, s: &[u8]) -> bool {
        if s.len() >= MAX_ARG_STRLEN || self.footprint() + s.len() + 1 + 8 > EXEC_ARG_MAX { return false; }
        let start = self.data.len();
        self.data.extend_from_slice(s);
        self.data.push(0);
        self.commit(start);
        true
    }

    fn reset(&mut self) {
        self.data.clear(); self.offsets.clear(); self.lengths.clear();
        self.end = 0; self.count = 0;
    }

    /// Empty the buffer and give back what an unusually large exec grew it
    /// to; the common few-KiB case keeps its allocation.
    fn release(&mut self) {
        self.reset();
        if self.data.capacity() > 64 * 1024 { self.data = Vec::new(); }
        if self.offsets.capacity() > 1024 { self.offsets = Vec::new(); self.lengths = Vec::new(); }
    }
}

/// A sleeping lock (lane fsio): argv/envp collection prefaults user pages,
/// and a fault on an absent demand-paged page reads the file through the
/// sleeping `F2FS_MOUNTS`; a spinlock held across that park would leave
/// every other execve spinning with IRQs masked.
static EXEC_ARGV: sched::ksync::KMutex<ExecStrBuf> = sched::ksync::KMutex::new(ExecStrBuf::new());

// ── `#!` scripts (Linux binfmt_script) ───────────────────────────────────────

/// Linux's `BINPRM_BUF_SIZE`: the `#!` line is parsed out of the first 256
/// bytes of the file and no further.
const SHEBANG_BUF: usize = 256;
/// Linux's `BINPRM_MAX_RECURSION`: a script may name a script as its
/// interpreter this many times; one more is ELOOP.
const SHEBANG_MAX_DEPTH: usize = 4;

/// The first `n` bytes of `path` (fewer if the file is shorter), or None when
/// it cannot be opened or read at all — in which case the caller lets the ELF
/// path report the failure it always has.
fn read_file_prefix(path: &str, n: usize) -> Option<alloc::vec::Vec<u8>> {
    let fd = open_kernel_path(path, 0 /* O_RDONLY */, 0);
    if fd < 0 { return None; }
    let fd = fd as usize;
    let mut buf = alloc::vec![0u8; n];
    let got = read_fd_upto(fd, &mut buf);
    let _ = sys_close(fd);
    if got < 0 { return None; }
    buf.truncate(got as usize);
    Some(buf)
}

/// Parse a `#!` line the way `binfmt_script.c` does. `buf` is the file's
/// first `SHEBANG_BUF` bytes (at most) and starts with `#!`.
///
/// Returns `(interpreter, optional single argument)`. The interpreter runs
/// from the first non-blank after `#!` to the first blank; everything after
/// the following blanks up to the end of the line, trailing blanks trimmed,
/// is ONE argument (Linux does not split it further). A NUL ends the line
/// early. Without a newline inside the buffer the interpreter path must still
/// be terminated by a blank inside it, or it may be truncated and the exec is
/// refused (ENOEXEC); the argument may be truncated, the interpreter re-reads
/// the script anyway.
fn parse_shebang(buf: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    let blank = |b: u8| b == b' ' || b == b'\t';
    let body = &buf[2..];
    let line_end = match body.iter().position(|&b| b == b'\n') {
        Some(i) => i,
        None => {
            let start = body.iter().position(|&b| !blank(b))?;
            body[start..].iter().position(|&b| blank(b) || b == 0)?;
            body.len()
        }
    };
    let mut line = &body[..line_end];
    if let Some(z) = line.iter().position(|&b| b == 0) { line = &line[..z]; }
    while let Some(&last) = line.last() {
        if blank(last) { line = &line[..line.len() - 1]; } else { break; }
    }
    let start = line.iter().position(|&b| !blank(b))?;
    let rest = &line[start..];
    match rest.iter().position(|&b| blank(b)) {
        None => Some((rest, None)),
        Some(i) => {
            let interp = &rest[..i];
            let arg = rest[i..].iter().position(|&b| !blank(b)).map(|k| &rest[i + k..]);
            Some((interp, arg))
        }
    }
}

/// Exec permission for a script: some execute bit must be set (EACCES
/// otherwise), asked of the owning filesystem through `VFS_ACCESS` so ACLs
/// count, with the `VFS_STAT` mode-bit fallback `sys_faccessat` uses for a
/// filesystem that predates it. Linux checks this at `open_exec` for every
/// binary; here it is applied to `#!` scripts, whose exec bit is the only
/// thing that distinguishes a program from a data file the interpreter
/// would otherwise happily run.
/// The execute-permission gate for execve: `access(X_OK)` on the file, as the
/// caller (owner/group/other bits, a stored ACL, supplementary groups; root
/// still needs at least one x bit). Used for the `#!` script and, since the
/// ELF loader never asked, for the final binary as well — a 0644 ELF used to
/// exec fine for anyone who could read it. A filesystem that cannot answer
/// (ENOSYS: the initrd) falls back to the stat mode bits.
fn exec_permitted(kp: &KPath, pid: u32) -> isize {
    const X_OK: u64 = 1;
    let amsg = make_vfs_msg(vfs::VFS_ACCESS, &[kp.ptr() as u64, X_OK]);
    let ar = vfs_reply_val(&vfs::handle(&amsg, pid));
    // Only a refusal is an answer: a missing file (ENOENT) or a filesystem
    // that cannot say (ENOSYS) leaves the loader to report its own errno.
    if ar != -38 { return if ar == -13 { -13 } else { 0 }; }
    let mut stat_buf = [0u8; STAT_SIZE];
    let msg = make_vfs_msg(vfs::VFS_STAT, &[kp.ptr() as u64, stat_buf.as_mut_ptr() as u64]);
    if vfs_reply_val(&vfs::handle(&msg, pid)) < 0 { return 0; } // unknowable: as before
    let st_mode = vfs::read_stat_mode(stat_buf.as_ptr() as usize);
    if st_mode & 0o111 == 0 { -13 } else { 0 }
}
/// The ids a set-user-ID / set-group-ID file grants on exec: `(Some(owner),
/// _)` for S_ISUID, `(_, Some(group))` for S_ISGID with the group-execute
/// bit (without it S_ISGID means mandatory locking, not set-group-ID). None
/// when the bit is clear, the file cannot be stat'ed, or the process set
/// PR_SET_NO_NEW_PRIVS, which Linux defines as "execve never grants
/// privileges". The setuid launcher of the Claude Code CLI (/bin/claude,
/// 04755 root) relies on the bit: before this the kernel ignored it, so a
/// setuid program simply ran with its caller's ids.
fn exec_setid(kp: &KPath, pid: u32) -> (Option<u32>, Option<u32>) {
    const S_ISUID: u32 = 0o4000;
    const S_ISGID: u32 = 0o2000;
    const S_IXGRP: u32 = 0o010;
    if sched::no_new_privs() { return (None, None); }
    // Inside a user namespace the set-id bits never grant host ids (stricter than
    // Linux, which maps the owner): credentials are host ids.
    if sched::ns::current_userns() != 0 { return (None, None); }
    // MS_NOSUID (Linux `mnt_may_suid`): the file's mount, or any alias
    // mount the path went through, ignores the set-ID bits.
    let mut crossed = false;
    let _ = backing_walk(kp.ns_bytes(), &mut crossed);
    if crossed || vfs::path_on_nosuid_mount(kp.bytes()) { return (None, None); }
    let mut stat_buf = [0u8; STAT_SIZE];
    let msg = make_vfs_msg(vfs::VFS_STAT, &[kp.ptr() as u64, stat_buf.as_mut_ptr() as u64]);
    if vfs_reply_val(&vfs::handle(&msg, pid)) < 0 { return (None, None); }
    let mode = vfs::read_stat_mode(stat_buf.as_ptr() as usize);
    let (uid, gid) = vfs::read_stat_owner(stat_buf.as_ptr() as usize);
    let su = if mode & S_ISUID != 0 { Some(uid) } else { None };
    let sg = if mode & S_ISGID != 0 && mode & S_IXGRP != 0 { Some(gid) } else { None };
    (su, sg)
}

static EXEC_ENVP: sched::ksync::KMutex<ExecStrBuf> = sched::ksync::KMutex::new(ExecStrBuf::new());

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
pub(crate) fn read_fd_upto(fd: usize, buf: &mut [u8]) -> isize {
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

    // Ensure the PT_INTERP path string is also in the buffer.  It usually sits
    // immediately after the program-header table, but that is not guaranteed;
    // a dynamic binary is only ever loaded through this header, so the kernel
    // must be able to read the interpreter path out of it (see sys_execve).
    let mut interp_end = 0usize;
    for i in 0..phnum {
        let po = phoff + i * phentsize;
        if po + 40 > hdr.len() { break; }
        let p_type = u32::from_le_bytes(hdr[po..po + 4].try_into().unwrap());
        if p_type == 3 /* PT_INTERP */ {
            let p_offset = u64::from_le_bytes(hdr[po + 8..po + 16].try_into().unwrap()) as usize;
            let p_filesz = u64::from_le_bytes(hdr[po + 32..po + 40].try_into().unwrap()) as usize;
            interp_end = p_offset.saturating_add(p_filesz);
        }
    }
    if interp_end > hdr.len() {
        if interp_end > EXEC_HEADER_MAX {
            let _ = sys_close(fd);
            return None;
        }
        hdr = alloc::vec![0u8; interp_end];
        let _ = sys_lseek(fd, 0, 0);
        if read_fd_upto(fd, &mut hdr) < interp_end as isize {
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

/// Open the dynamic interpreter for demand paging: its ELF headers, and a
/// private-mmap registry cap (one reference, the caller's) naming its inode.
/// None when it is not an f2fs file (or the registry is full): the caller
/// falls back to the eager loader.
fn open_interp_lazy(path: &str, pid: u32) -> Option<(usize, alloc::vec::Vec<u8>)> {
    let (fd, hdr) = open_exec_header(path, pid)?;
    let cap = match vfs::vfs_get_node_kind(pid, fd) {
        Some(vfs::VnodeKind::MountedFile { port, file_id }) =>
            f2fs_server::inode_by_port(port, file_id as u64)
                .and_then(|ino| mmap_file_register(port, ino)),
        _ => None,
    };
    let _ = sys_close(fd);
    Some((cap?, hdr))
}

/// A host-absolute *namespace* path as the calling task sees it: the chroot
/// prefix stripped (getcwd's translation). Unjailed tasks, and paths not
/// under the jail root, come back unchanged.
fn jail_relative(p: &[u8]) -> alloc::vec::Vec<u8> {
    if let Some(r) = sched::current_root_arc() {
        if r.len() > 1 && vfs::alias::under(p, &r) {
            if p.len() == r.len() { return b"/".to_vec(); }
            return p[r.len()..].to_vec();
        }
    }
    p.to_vec()
}

fn sys_execve(path_ptr: usize, argv_ptr: usize, envp_ptr: usize) -> isize {
    let pid = current_pid();

    // Resolve against the cwd through the shared helper, so `./prog` and
    // `prog` name the same file execve sees as every other path syscall.
    let mut kpath = match resolve_user_path(path_ptr) { Ok(p) => p, Err(e) => return e };
    // execve follows a final /proc magic link like open does:
    // `execv("/proc/self/exe")` re-runs the binary itself (and fd/N of a
    // mounted file runs that file) instead of failing on the link.
    kpath = match follow_magic_final(kpath) { Ok(p) => p, Err(e) => return e };
    // MS_NOEXEC (Linux `path_noexec`): the file named, script or binary.
    if vfs::path_on_noexec_mount(kpath.bytes()) { return -13; } // EACCES

    // ── `#!` scripts ─────────────────────────────────────────────────────────
    // A file starting with `#!` names its interpreter; exec that instead, with
    // argv rewritten as Linux's binfmt_script does: the caller's argv[0] is
    // dropped and `[interpreter, argument?, script-path]` goes in front of
    // argv[1..]. `script-path` is the resolved absolute path (the caller's
    // cwd is what it was resolved against, so the interpreter finds the same
    // file). Nesting — an interpreter that is itself a script — repeats the
    // rewrite: `[i2, a2?, i1, a1?, script, argv[1..]]`, up to
    // SHEBANG_MAX_DEPTH levels.
    let mut head_args: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    let mut depth = 0usize;
    loop {
        let cur = match core::str::from_utf8(kpath.bytes()) {
            Ok(s) => s,
            Err(_) => return -2,
        };
        let head = match read_file_prefix(cur, SHEBANG_BUF) {
            Some(h) => h,
            None => break, // unreadable: the ELF path below reports it
        };
        if head.len() < 2 || &head[..2] != b"#!" { break; }
        if depth >= SHEBANG_MAX_DEPTH { return -40; } // ELOOP
        let perm = exec_permitted(&kpath, pid);
        if perm != 0 { return perm; }
        // ...and a `#!` interpreter on a noexec mount is refused too.
        if vfs::path_on_noexec_mount(kpath.bytes()) { return -13; } // EACCES
        let (interp, iarg) = match parse_shebang(&head) {
            Some(x) => x,
            None => return -8, // ENOEXEC
        };
        let mut next: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
        next.push(interp.to_vec());
        if let Some(a) = iarg { next.push(a.to_vec()); }
        // The script path as the TASK sees it: kpath is host-absolute, so a
        // chrooted caller must get the jail-relative form, or the interpreter
        // re-resolves "<root>/<root>/script" and fails with ENOENT.
        next.push(jail_relative(kpath.ns_bytes()));
        // The previous level's interpreter name (head_args[0]) is the file we
        // just parsed and was re-added above as its own script path.
        next.extend(head_args.into_iter().skip(1));
        head_args = next;

        kpath = match resolve_ns(interp).and_then(KPath::from_ns) {
            Ok(k) => k,
            Err(e) => return e,
        };
        depth += 1;
    }
    let script = !head_args.is_empty();

    // The binary itself (or the script's interpreter) needs execute permission.
    // A missing file still reports ENOENT below, not EACCES: exec_permitted
    // only rules when the file exists and the caller lacks x on it.
    {
        let perm = exec_permitted(&kpath, pid);
        if perm != 0 { return perm; }
        // ...and a `#!` interpreter on a noexec mount is refused too.
        if vfs::path_on_noexec_mount(kpath.bytes()) { return -13; } // EACCES
    }
    // Set-user-ID / set-group-ID (Linux `bprm_fill_uid`): taken from the
    // file actually loaded, the interpreter of a `#!` script, never the
    // script itself. Applied past the point of no return below.
    let (setid_uid, setid_gid) = exec_setid(&kpath, pid);
    let (cur_uid, cur_gid) = (sched::current_uid(), sched::current_gid());
    let new_euid = setid_uid.unwrap_or_else(sched::current_euid);
    let new_egid = setid_gid.unwrap_or_else(sched::current_egid);
    // AT_SECURE: the new image runs with ids its invoker does not have, so
    // the dynamic loader and libc must distrust the environment (LD_*).
    let at_secure = new_euid != cur_uid || new_egid != cur_gid;

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
        // fd tables are per-process (TGID): execve from a non-leader thread must
        // resolve the fd in the process table, matching the cloexec sweep's
        // sched::tgid_of below (else steal returns None → demand-paging fast path
        // is silently skipped).
        match vfs::steal_mounted_file(sched::tgid_of(pid), fd) {
            Some((port, file_id)) => match exec_file_register(port, file_id) {
                Some(cap) => {
                    file_name_note(cap, kpath.bytes());
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
            // Silent: ENOENT here is normal PATH-probing (see EXECVE dispatch).
            return -2; // ENOENT
        }
    };

    if exec_cap == 0 && elf_len == 0 { return -22; }

    // The image and its interpreter are loaded into the new address space
    // BEFORE argv/envp are collected. The collection holds the global
    // EXEC_ARGV/EXEC_ENVP buffers, and the loaders' file I/O (headers; the
    // eager fallback's whole-file reads) used to run under them, making
    // every other execve in the system wait on it. The new address space is
    // private to this call until replace_address_space, so no lock is needed.
    // ── Load ELF into fresh address space ─────────────────────────────────────
    let pt_root = unsafe { arch_alloc_page_table_root() };
    if pt_root == 0 {
        if exec_cap != 0 { exec_file_release(exec_cap); }
        return enomem_site("execve/page-table-root");
    }
    let mut new_as = alloc::boxed::Box::new(mm::vmm::AddressSpace::new(pt_root));

    // Header bytes for inspection: the full image (eager) or just the ELF +
    // program-header + interp-string prefix (demand-paged).
    let elf_bytes: &[u8] = if exec_cap != 0 {
        header_data.as_deref().unwrap_or(&[])
    } else {
        unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_len) }
    };

    // ET_DYN images (PIE, static-PIE, dynamic) load at a non-zero bias; ET_EXEC
    // keeps bias 0, byte-identical to the path the static userland has always
    // taken (bias collapses every `bias + x` back to `x`).
    let e_type = if elf_bytes.len() >= 18 {
        u16::from_le_bytes([elf_bytes[16], elf_bytes[17]])
    } else { 0 };
    let bias = if e_type == ET_DYN { MAIN_DYN_BASE } else { 0 };

    let elf_info = if exec_cap != 0 {
        // Demand-paged: map PT_LOADs as file-backed lazy VMAs (each takes a
        // reference on exec_cap), no segment data read here.
        let r = elf::load_lazy(elf_bytes, &mut new_as, exec_cap, bias);
        // Drop the creation reference: the image's lifetime is now carried
        // by the VMAs (dropping new_as on the error paths below releases
        // theirs, letting the refcount reach zero and close the file).
        exec_file_release(exec_cap);
        match r {
            Ok(e)  => e,
            Err(_) => { drop(new_as); return -8; }
        }
    } else {
        match elf::load(elf_bytes, &mut new_as, bias) {
            Ok(e)  => e,
            Err(_) => { drop(new_as); return -8; }
        }
    };

    // Heap follows the MAIN image. Capture it now: loading the interpreter
    // below runs the loader again and would otherwise reset heap_start to sit
    // after INTERP_BASE (768 MiB) instead of after the ~2 MiB main image.
    let main_heap = new_as.heap_start;

    // ── Dynamic interpreter (PT_INTERP) ────────────────────────────────────────
    // A dynamic binary names an interpreter (musl's ld.so, itself ET_DYN). The
    // kernel maps it at INTERP_BASE alongside the main image and enters at *its*
    // entry point; ld.so self-relocates (using AT_BASE), then processes the main
    // program from the auxv and jumps to AT_ENTRY. Static (ET_EXEC) and
    // static-PIE (ET_DYN, no PT_INTERP) binaries have no interpreter: the kernel
    // enters their own entry directly and AT_BASE stays 0 (musl's rcrt1
    // self-relocates from AT_PHDR).
    let mut entry   = elf_info.entry;   // no-interp default: jump straight in
    let mut at_base = 0u64;
    if let Some((ioff, ilen)) = elf_info.interp {
        if ilen == 0 || ioff.saturating_add(ilen) > elf_bytes.len() {
            drop(new_as); return -8;
        }
        let raw = &elf_bytes[ioff..ioff + ilen];
        let plen = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        // Copy the path out of the header buffer so nothing borrows it across
        // the interpreter load below.
        // Resolved like any other path the task names, i.e. under its chroot:
        // a jailed Alpine binary's /lib/ld-musl-<arch>.so.1 is the JAIL's
        // musl, not the image's libc (a different musl build/revision and
        // file). For an unjailed task this is the same absolute path as
        // before.
        if plen == 0 || plen >= KPATH_MAX { drop(new_as); return -8; }
        let interp_kp = match resolve_ns(&raw[..plen]).and_then(KPath::from_ns) {
            Ok(k) => k,
            Err(_) => { drop(new_as); return -2; }
        };
        let interp_path = match core::str::from_utf8(interp_kp.bytes()) {
            Ok(s) => s,
            Err(_) => { drop(new_as); return -8; }
        };

        // Preferred: demand-paged, like the main image. The interpreter is
        // the same file for every dynamic process, so it goes through the
        // deduplicated, inode-keyed private-mmap registry (one entry for all
        // of them, no open-file slot held) rather than EXEC_FILES. Nothing is
        // read here beyond its headers; the eager whole-file read and copy
        // below (~1 MiB of libc per exec) is only the fallback.
        if let Some((icap, ihdr)) = open_interp_lazy(interp_path, pid) {
            let r = elf::load_lazy(&ihdr, &mut new_as, icap, INTERP_BASE);
            mmap_file_ref(icap, false); // the VMAs hold their own references
            match r {
                Ok(ii) => { entry = ii.entry; at_base = INTERP_BASE as u64; }
                Err(_) => { drop(new_as); return -8; }
            }
        } else {
        match read_file_from_vfs(interp_path) {
            Some(ibytes) => match elf::load(&ibytes, &mut new_as, INTERP_BASE) {
                Ok(ii) => { entry = ii.entry; at_base = INTERP_BASE as u64; }
                Err(_) => { drop(new_as); return -8; }
            },
            None => {
                serial_print_str("[EXEC] dynamic interpreter not found: ");
                serial_print_str(interp_path);
                serial_print_str("\n");
                drop(new_as); return -8;
            }
        }
        }
    }
    // Pin the heap to the main image regardless of whether the interpreter load
    // moved it.
    new_as.heap_start = main_heap;
    new_as.heap_end   = main_heap;

    // The loaders are done with the file bytes (eager: copied into the new
    // address space; lazy: only the header was ever read).  Free the buffers
    // explicitly: replace_address_space below never returns, so destructors
    // of locals still alive at that call never run, and the eager buffer is
    // the size of the whole ELF (hundreds of MB for large binaries).
    drop(vfs_elf_data);
    drop(header_data);

    // ── Collect argv / envp strings ───────────────────────────────────────────
    let mut argv = EXEC_ARGV.lock();
    let mut envp = EXEC_ENVP.lock();
    argv.reset();
    envp.reset();

    let t_pre = mm::paging::tlbstat::now_ns();
    // A script's rewritten head goes first (see the `#!` loop above).
    for a in &head_args {
        if !argv.push_bytes(a) { return -7; } // E2BIG
    }
    drop(head_args);
    // Read argv[] then envp[] from user space (NULL-terminated pointer
    // arrays). For a script, argv[0] was replaced by the head above and is
    // skipped. Each pointer is faulted in and read fault-checked (the arrays
    // can live in .data/.rodata of a demand-paged image, not just on the
    // stack); a bad one is EFAULT and running past EXEC_ARG_MAX is E2BIG, as
    // on Linux, where both used to end the list early without an error.
    let read_ptr = |addr: usize| -> Result<usize, isize> {
        const PW: usize = core::mem::size_of::<usize>();
        if !validate_user_buf(addr, PW) { return Err(-14); }
        prefault_user_ro(addr, PW);
        let mut v: usize = 0;
        let ok = with_current_address_space(|as_| {
            as_.read_user_buf(addr, unsafe {
                core::slice::from_raw_parts_mut(&mut v as *mut usize as *mut u8, PW)
            })
        }).unwrap_or(false);
        if ok { Ok(v) } else { Err(-14) }
    };
    for (vec_ptr, is_env) in [(argv_ptr, false), (envp_ptr, true)] {
        if vec_ptr == 0 { continue; }
        let mut i = 0usize;
        loop {
            let str_ptr = match read_ptr(vec_ptr + i * core::mem::size_of::<usize>()) {
                Ok(p) => p,
                Err(e) => return e,
            };
            if str_ptr == 0 { break; }
            if !is_env && script && i == 0 { i += 1; continue; }
            let budget = EXEC_ARG_MAX.saturating_sub(argv.footprint() + envp.footprint());
            let buf = if is_env { &mut *envp } else { &mut *argv };
            if let Err(e) = buf.push_cstr(str_ptr, budget) { return e; }
            i += 1;
        }
    }
    let argc = argv.count;
    let envc = envp.count;
    {
        use mm::paging::tlbstat as ts;
        ts::EXEC_PRE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        ts::add(&ts::EXEC_PRE_NS, &ts::EXEC_PRE_MAX_NS, ts::now_ns().saturating_sub(t_pre));
    }


    // Reserve the user stack. It is demand-paged like any anonymous private
    // mapping: only the pages holding argv/envp/auxv (plus a small prefault
    // below the initial SP, see STACK_PREFAULT) are populated here, the rest
    // on first touch. The VMA is fixed at USER_STACK_SIZE; nothing is mapped
    // below it, so running off its bottom faults outside any VMA and raises
    // SIGSEGV (the guard). Until 2026-09-26 all 8 MiB were allocated eagerly
    // at every exec — ~8 MiB of untouched RSS per process.
    let stack_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE;
    let stack_size = exec_stack_size();
    if !new_as.map_lazy(USER_STACK_TOP - stack_size, stack_size, stack_flags, false) {
        let r = enomem_map_site("execve/user-stack");
        drop(new_as); return r;
    }
    new_as.stack_top = USER_STACK_TOP;

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
    // auxv pairs: PHDR + PHENT + PHNUM + RANDOM + PAGESZ + UID + EUID + GID +
    // EGID + VFS + NET + AUDIO + ENTRY + BASE + SECURE + HWCAP + EXECFN +
    // RSEQ_FEATURE_SIZE + RSEQ_ALIGN + NULL = 20 pairs. ENTRY..EXECFN are
    // mandatory for the dynamic (PT_INTERP) path and harmless standard tags
    // for static binaries.
    let auxv_words = 20 * 2;
    let total_words = ptr_words + auxv_words;
    let total_ptr_bytes = total_words * W;
    // Align string section to 16 bytes.
    let str_section = argv_str_total + envp_str_total + rand_bytes;
    let str_aligned = (str_section + 15) & !15;

    let frame_size = total_ptr_bytes + str_aligned;
    if frame_size > stack_size { return -22; } // EINVAL — too many args

    let user_sp = USER_STACK_TOP - frame_size;
    // Align sp to 16 bytes (ABI requirement on AArch64 and x86-64).
    let user_sp = user_sp & !15;

    // String base in user VA — starts right after the pointer table.
    let str_base_va = user_sp + total_ptr_bytes;
    let rand_va     = str_base_va + argv_str_total + envp_str_total;

    // Populate the frame's pages plus STACK_PREFAULT below the initial SP
    // (the first few call frames of ld.so / _start, which would otherwise
    // each take a fault straight away).
    let prefault_lo = user_sp.saturating_sub(STACK_PREFAULT).max(USER_STACK_TOP - stack_size);
    new_as.prefault_range(prefault_lo, USER_STACK_TOP - prefault_lo);

    // Build the frame [user_sp, USER_STACK_TOP) in a kernel buffer, then copy
    // it in page by page (the stack's frames are no longer one contiguous
    // physical block).
    let mut frame: Vec<u8> = alloc::vec![0u8; USER_STACK_TOP - user_sp];
    let fb = frame.as_mut_ptr();
    let write64 = |off: usize, val: u64| unsafe {
        core::ptr::copy_nonoverlapping(val.to_le_bytes().as_ptr(), fb.add(off), 8);
    };
    let write8 = |off: usize, src: *const u8, len: usize| unsafe {
        core::ptr::copy_nonoverlapping(src, fb.add(off), len);
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
    let auxv: &[(u64, u64)] = &[
        (3,  elf_info.phdr_va   as u64),           // AT_PHDR
        (4,  elf_info.phentsize as u64),            // AT_PHENT
        (5,  elf_info.phnum     as u64),            // AT_PHNUM
        (25, rand_va as u64),                       // AT_RANDOM
        (6,  mm::buddy::PAGE_SIZE as u64),          // AT_PAGESZ
        (11, cur_uid as u64),                       // AT_UID
        (12, new_euid as u64),                      // AT_EUID
        (13, cur_gid as u64),                       // AT_GID
        (14, new_egid as u64),                      // AT_EGID
        (AT_LEANDROS_VFS_PORT, VFS_SERVER_PORT.load(Ordering::Relaxed) as u64),
        (AT_LEANDROS_NET_PORT, NET_SERVER_PORT.load(Ordering::Relaxed) as u64),
        (AT_LEANDROS_AUDIO_PORT, AUDIO_SERVER_PORT.load(Ordering::Relaxed) as u64),
        (9,  elf_info.entry as u64),                // AT_ENTRY — main exe entry (biased)
        (7,  at_base),                              // AT_BASE  — interpreter load base, 0 if none
        (23, at_secure as u64),                     // AT_SECURE
        (16, linux_hwcap()),                        // AT_HWCAP
        // AT_EXECFN → argv[0] string (musl falls back to argv[0] if absent, so
        // 0 when there are no args is fine).
        (31, if argc > 0 { str_base_va as u64 } else { 0 }),
        // rseq(2): glibc sizes and aligns each thread's area from these.
        (27, sched::rseq::RSEQ_FEATURE_SIZE as u64), // AT_RSEQ_FEATURE_SIZE
        (28, sched::rseq::RSEQ_ALIGN as u64),        // AT_RSEQ_ALIGN
        (0,  0),                                    // AT_NULL
    ];
    for &(k, v) in auxv {
        write64(w * W, k); w += 1;
        write64(w * W, v); w += 1;
    }

    // String data section.
    write8(total_ptr_bytes, argv.data.as_ptr(), argv_str_total);
    write8(total_ptr_bytes + argv_str_total, envp.data.as_ptr(), envp_str_total);

    // AT_RANDOM data: 16 bytes from the kernel CSPRNG (sched::random — seeded
    // from virtio-rng, the DTB rng-seed, RNDR/RDRAND and cycle-counter
    // jitter). libc derives the stack canary and pointer guard from these, so
    // they must be unpredictable; they used to be a function of the tick
    // count, which any process could read or guess.
    let rand_off = total_ptr_bytes + argv_str_total + envp_str_total;
    let mut at_random = [0u8; 16];
    sched::random::fill(&mut at_random);
    write8(rand_off, at_random.as_ptr(), 16);
    for b in at_random.iter_mut() { unsafe { core::ptr::write_volatile(b, 0); } }

    if !new_as.write_user_buf(user_sp, &frame) {
        drop(new_as); return enomem_site("execve/stack-not-backed");
    }
    drop(frame);

    // Release argv/envp buffers before the AS swap.
    argv.release();
    envp.release();
    drop(argv);
    drop(envp);

    // ── VFS lifecycle and address space replacement ────────────────────────────
    // POSIX execve terminates every *other* thread in the caller's group: the
    // old program image (their code and stacks) is about to be replaced, so any
    // sibling left running would execute freed/stale mappings. Without it, a
    // COSMIC component that re-exec'd with a live worker thread orphaned that
    // worker on the old address space; its next fault resolved against the
    // leader's *new* AS and killed it, and freeing the old AS out from under it
    // faulted the page-table walk (M7o).
    //
    // Everything from here on is infallible, so this is the point of no
    // return and no sibling is reaped on a path that could still fail. It
    // must come BEFORE the close-on-exec sweeps below, as Linux's de_thread
    // precedes do_close_on_exec: the sweeps close fds the siblings are still
    // using, and a sibling alive across them sees its fds vanish. That was
    // tokio's "Bad read on self-pipe: EBADF" panic in brush whenever it ran
    // `exec <cmd>` (greetd's session wrapper): closing the SOCK_CLOEXEC signal
    // socketpair woke a worker parked in epoll_wait on it, and its read of the
    // already-closed receiver failed EBADF before the kill reached it.
    //
    // A non-leader caller takes over the leader's pid here, so `pid` is
    // re-read; the tgid (the fd-table key) does not change.
    // Robust futexes of the old image: the caller's own list is walked under
    // its current tid (the dethread may give it the leader's), the killed
    // siblings' lists right after, both while the old address space is
    // still mapped (Linux: exec_mm_release).
    sched::robust::release_current();
    let pre_tid = current_pid();
    sched::dethread_current_group();
    // exec unshares: a private table becomes the process table.
    if let Some((key, tgid, _)) = sched::fdkey::drop(pre_tid) {
        for (_, h) in vfs::handle_fds(tgid) { close_handle(pre_tid, h); }
        let m = make_vfs_msg(vfs::VFS_CLOSE_ALL, &[tgid as u64]);
        let _ = vfs::handle(&m, tgid);
        vfs::rekey_table(key, tgid);
    }
    sched::robust::release_orphans(sched::current_tgid());
    let pid = current_pid();
    // The new image's credentials: the set-ID bits of the file, and the
    // saved ids following the effective ones (see exec_setid).
    sched::exec_commit_ids(setid_uid, setid_gid, at_secure);
    // Linux clears the parent-death signal when exec changes credentials.
    if setid_uid.is_some() || setid_gid.is_some() { sched::set_pdeath_signal(0); }

    // VFS_EXEC_CLOEXEC takes the owning pid explicitly and so bypasses the
    // tgid canonicalization vfs::handle applies to ordinary calls. fd tables
    // are keyed by tgid, so an execve issued from a non-leader thread (a tokio
    // worker, say) named a pid no table matches and closed nothing at all —
    // leaking every close-on-exec fd, including the pipe ends whose closure is
    // what signals EOF to the reader. sys_clone_or_fork already gets this right.
    let fd_owner = sched::tgid_of(pid);
    trace_fd("execve cloexec-sweep", pid as usize, fd_owner as usize, 0, 0);
    let cloexec_msg = make_vfs_msg(vfs::VFS_EXEC_CLOEXEC, &[fd_owner as u64]);
    let _ = vfs::handle(&cloexec_msg, fd_owner);
    let net_cloexec = make_vfs_msg(net_server::NET_EXEC_CLOEXEC, &[fd_owner as u64]);
    let _ = net_server::handle(&net_cloexec, fd_owner);
    epoll_exec_cloexec(fd_owner);
    // A socket's or an epoll fd's close-on-exec flag lives on its handle,
    // which the sweeps above just closed if it was set; drop the descriptors
    // left naming nothing.
    vfs::prune_handles(fd_owner, |h| handle_alive(fd_owner, h));

    // A CLONE_VFORK child stops borrowing the parent's address space here —
    // release the parent from its vfork suspension (POSIX: parent resumes on
    // the child's successful exec or exit).
    sched::vfork_complete(pid);

    // Record the executable path for /proc/self/exe (tgid-keyed side table);
    // kpath is the resolved absolute path. Must precede replace_address_space,
    // which never returns.
    sched::set_exe_path(fd_owner, kpath.bytes());
    // Where argv/envp strings sit on the new stack: /proc/<pid>/cmdline and
    // environ read them back from there (see sched::read_proc_args).
    sched::set_arg_area(fd_owner, sched::ArgArea {
        arg_start: str_base_va,
        arg_end: str_base_va + argv_str_total,
        env_start: str_base_va + argv_str_total,
        env_end: str_base_va + argv_str_total + envp_str_total,
    });
    if SC_STATS && kpath.bytes().ends_with(b"cosmic-comp") {
        sched::SC_FOCUS_TGID.store(fd_owner, Ordering::Relaxed);
    }
    if SC_STATS && kpath.bytes().ends_with(b"cosmic-greeter-login") {
        sched::SC_FOCUS2_TGID.store(fd_owner, Ordering::Relaxed);
    }

    // POSIX execve: caught signal handlers revert to SIG_DFL in the new image
    // (SIG_IGN/SIG_DFL and the signal mask are preserved). Omitting this let a
    // stale handler survive the exec; since /bin/sh is a hardlink to brush (same
    // fixed-base ET_EXEC), the handler reappeared at the same address and
    // signal_hook_registry chained to itself on the next signal — an unbounded,
    // stack-exhausting recursion that blocked the COSMIC session launcher.
    sched::reset_handlers_on_exec(fd_owner);

    // Unbox in an inner scope: `replace_address_space` never returns, so a
    // `Box` still alive in this frame is never deallocated — `*new_as` as the
    // argument moved the value out but left the 56-byte box allocation to a
    // scope end that never comes, one slab object per exec.
    let new_as: mm::vmm::AddressSpace = { let b = new_as; *b };
    // `replace_address_space` never returns, so nothing this frame still owns
    // is ever dropped: the resolved path (one or two small Vecs) leaked on
    // every successful exec until 2026-10-23 (openbugs L1.9).
    drop(kpath);
    replace_address_space(new_as, pt_root, heap_start, entry, user_sp);
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

use crate::CONSOLE_OUT_LOCK;

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

    {
        let mut q = PENDING_INPUT.lock();
        q.push(0x1b);
        q.push(b'[');
        push_dec(&mut q, row);
        q.push(b';');
        push_dec(&mut q, col);
        q.push(b'R');
    }
    wake_console_input();
}

/// Wake the readers of console input after a push into `PENDING_INPUT`.
/// Blocking console reads (`console_read_park`) and poll/epoll/select
/// interests on the console (`console_poll_tag`) both name this key. A KEYED
/// wake: a poller that only shares `POLL_TAG_CONSOLE_IN`'s bloom bit through
/// some other object is not woken (broadcast-mask parks still are).
/// Never blocks (callers may hold console locks): under a contended RUN_QUEUE
/// the wake is deferred, still keyed, to the next tick.
fn wake_console_input() {
    if !sched::try_wake_poll_key(sched::POLL_KEY_CONSOLE_IN) {
        sched::request_poll_wake_key(sched::POLL_KEY_CONSOLE_IN);
    }
}

/// Park a blocking console-stdin read (phase 1, with `deadline`) on exactly what can
/// make `read_input_byte` return a byte. That is the console tap's queue on the
/// keyboard evdev node (keyboard keys and UART bytes, both pushed by
/// `evdev_server::push_event` and woken by its keyed `wake_dev` at the end of
/// every input drain) and `PENDING_INPUT` (`wake_console_input`). The other
/// ways out are woken without a tag: a signal (SIGINT from ^C, SIGHUP on
/// hangup, SIGTTIN/SIGCONT) readies the blocked thread directly as it is
/// posted, and a VT switch completes with a broadcast. The read keeps a deadline
/// (`CONSOLE_READ_BACKSTOP_NS`) as the backstop, so a wake that is ever missed costs at
/// most 1 s, never a dead prompt.
///
/// It used to park broadcast (`block_on_poll_prepare_until`), so every poll
/// wake in the system woke the idle `login` prompt: ~3.3k wakes/s on the
/// laptop during video. Then it parked with the bare bloom mask
/// (`block_on_poll_prepare_masked`), which leaves both bits unrefined: every
/// keyed pipe / AF_UNIX / eventfd wake whose key hashed to one of the two bits
/// (~3 % of them) still woke it, ~450/s per getty on the laptop (lane
/// gettywake). It now parks with the keys (`block_on_poll_prepare_tag`, the
/// blocked-read helper of lane sfdflake), so a keyed wake reaches
/// it only on the evdev node's or `PENDING_INPUT`'s own key; bit-only and
/// broadcast wakes are unaffected by the refinement and still reach it.
/// Backstop deadline of a blocking console read's park (lane wakescan). Every
/// producer of console input ends in a keyed wake that reaches the park (see
/// `console_poll_tag`), so the deadline only bounds a wake that is ever
/// missed. It was one tick (10 ms): an idle serial shell then woke ~100
/// times a second just to find nothing; 1 s keeps the backstop at 1 wake/s.
const CONSOLE_READ_BACKSTOP_NS: u64 = 1_000_000_000;

pub(crate) fn console_read_park(deadline: u64) {
    sched::block_on_poll_prepare_tag(deadline, &console_poll_tag(true));
}

/// The poll tag of a console stdio interest (fd 0-2 on the console, `/dev/tty`,
/// a dup'd stdin, `/dev/ttyN`) in poll/epoll/select, and, through
/// `console_read_park`, of a blocking console read: one helper for every park
/// on console input, so they cannot drift from the producers.
///
/// `input`: the interest's readiness is console input (`poll_fd_state`'s
/// `console_input_pending() || has_key_event(0) || serial_has_data()`). Its
/// producers, all audited:
/// - the keyboard evdev node's queue: every `evdev_server::push_event` (virtio
///   and PS/2 keys, and UART bytes, which `serial_rx_drain` pushes from the
///   tick or the aarch64 UART IRQ) ends in a KEYED `wake_dev(0)` of
///   `evdev_poll_key(0)`, so the key is carried here and the wake is not
///   filtered out;
/// - `PENDING_INPUT`: `wake_console_input`, a keyed wake of
///   `POLL_KEY_CONSOLE_IN`;
/// - a byte still in the UART FIFO (`serial_has_data`) is not a park case: it
///   reports ready at once, and the drain that moves it is the evdev push
///   above.
///
/// Not `input`: console fd 1/2, whose readiness is a constant POLLOUT; nothing
/// ever changes it, so the static tag (no producer) is exact.
pub(crate) fn console_poll_tag(input: bool) -> sched::PollTag {
    if input {
        sched::PollTag::keyed(&[evdev_server::evdev_poll_key(0), sched::POLL_KEY_CONSOLE_IN], 0)
    } else {
        sched::PollTag::foreign(vfs::static_poll_tag())
    }
}

/// Console output from userspace, with `CSI 6 n` intercepted and answered
/// locally instead of forwarded to the serial line.
///
/// Everything else passes through untouched, so the serial log still shows the
/// full byte stream.  A query split across two `write` calls is not recognised
/// and falls back to the old behaviour (an attached terminal answers it).
///
/// `bytes` MUST already live in kernel memory: the whole call runs under
/// `CONSOLE_OUT_LOCK`, and touching user memory under a spinlock can take a
/// demand-page fault that re-enters the scheduler and freezes every vCPU.
fn console_write_user(bytes: &[u8]) {
    console_write_user_locked(bytes);
    // Kernel diagnostic lines queued while this write held the lock go out
    // now, between this write and the next — see `console_drain_outbox`.
    crate::console_drain_outbox();
}

fn console_write_user_locked(bytes: &[u8]) {
    const CPR_QUERY: &[u8] = b"\x1b[6n";
    let _out = CONSOLE_OUT_LOCK.lock();

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

/// Helper to read a single byte from evdev0 (unifying UART and keyboard).
fn read_input_byte() -> Option<u8> {
    // Synthetic input (the cursor-position reply, and the tail of a key that
    // produced more than one byte) is delivered ahead of the hardware queue so
    // it reaches the reader in the order it was generated.
    if let Some(b) = PENDING_INPUT.lock().pop() { return Some(b); }
    loop {
        if let Some(ev) = evdev_server::pop_event(0) {
            // EV_KEY
            if ev.type_ == 1 {
                if ev.value == 2 {
                    // value == 2 means this event carries a literal byte from
                    // serial input (see arch/*/timer.rs's on_tick), not a
                    // keyboard scancode. Pass everything through — dropping
                    // control bytes here swallowed the ESC of every ANSI escape
                    // sequence, so a terminal's cursor-position report
                    // ("\x1b[n;mR") arrived as "[n;mR" and crossterm's CPR parser
                    // never matched (brush bailed out of interactive mode).
                    // Line-discipline signal bytes (^C/^Z/^\) were already
                    // intercepted at the UART drain; UTF-8 lead/continuation
                    // bytes (>= 0x80) must survive too. ASCII '6' and '*' are
                    // also 54/42, the evdev codes for the two Shift keys, which
                    // is why serial bytes never reach the keymap below.
                    let c = ev.code;
                    if c > 0 && c <= 255 {
                        return Some(c as u8);
                    }
                    continue;
                }
                // A real key edge (press or release): VT 1's keymap
                // (servers/tty/src/keyboard.rs — Linux's default map, or
                // whatever loadkmap put there), in queue order, so Shift,
                // Ctrl, Alt, AltGr, CapsLock, NumLock, dead keys and the
                // cursor/function keys all mean what they mean on a Linux
                // console. Releases matter: they are how the modifier state
                // learns a key went up.
                let mut out = tty_server::keyboard::Out::new();
                tty_server::vt::console_translate(ev.code, ev.value, &mut out);
                let bytes = out.bytes();
                if bytes.is_empty() { continue; }
                if bytes.len() == 1 {
                    // The console has no ICRNL stage of its own (its input
                    // is raw bytes), and Enter has always arrived here as
                    // '\n'; the keymap's K_ENTER is the Linux '\r'.
                    let b = if bytes[0] == b'\r' { b'\n' } else { bytes[0] };
                    // Line-discipline ISIG intercept for hardware-keyboard
                    // input: ^C/^\/^Z become signals to the foreground
                    // process group instead of literal bytes.
                    if tty_server::console_intercept_byte(b) { continue; }
                    return Some(b);
                }
                {
                    let mut q = PENDING_INPUT.lock();
                    for &b in &bytes[1..] { q.push(b); }
                }
                // This reader returns bytes[0] now; the tail is for whoever
                // reads next, possibly another thread already parked.
                wake_console_input();
                return Some(bytes[0]);
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
/// O_APPEND serialisation (see `vfs::fd_append_lock_key`). Every write to an
/// O_APPEND regular file takes one of these sleeping locks, and `sys_writev`
/// holds it across the whole vector, so no other append can land between two
/// of its iovecs. Hashed: unrelated files may share a lock, which only costs
/// a short wait.
static APPEND_LOCKS: [sched::ksync::KMutex<()>; 16] =
    [const { sched::ksync::KMutex::new(()) }; 16];

fn append_lock_for(pid: u32, fd: usize) -> Option<&'static sched::ksync::KMutex<()>> {
    if fd >= net_server::SOCK_FD_BASE { return None; }
    vfs::fd_append_lock_key(pid, fd).map(|k| {
        let h = k.wrapping_mul(0x9E37_79B9) >> 28;
        &APPEND_LOCKS[h as usize & 15]
    })
}

fn sys_write(fd: usize, buf_ptr: usize, count: usize) -> isize {
    sys_write_impl(fd, buf_ptr, count, false)
}

/// `append_held`: the caller (writev) already holds this fd's append lock.
fn sys_write_impl(fd: usize, buf_ptr: usize, count: usize, append_held: bool) -> isize {
    if count == 0 { return 0; }

    if !validate_user_buf(buf_ptr, count) { return -14; }
    // Fault the source in before it reaches VFS/f2fs: taking a demand-page
    // fault inside the filesystem (its lock held) would deadlock, and the
    // fd 1/2 fast path's read_user_buf would fail with EFAULT on a
    // never-touched page (e.g. a .rodata string of a demand-paged binary).
    prefault_user(buf_ptr, pipe_prefault_len(current_pid(), fd, count, true));
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
            // Job control (TOSTOP): a background writer of its controlling
            // console takes SIGTTOU; see console_read_check for the shape.
            let jc = tty_server::console_write_check();
            if jc != 0 { return jc; }

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
        f if is_sock_handle(f) => {
            let msg = make_vfs_msg(net_server::NET_SEND,
                &[fd as u64, buf_ptr as u64, count as u64, 0, 0, 0]);
            net_block_lazy(pid, fd, false, true,
                || net_reply_val(&net_server::handle(&msg, pid)))
        }

        _ => {
            trace_fd_route(fd, "vfs");
            let _append = if append_held { None } else { append_lock_for(pid, fd).map(|m| m.lock()) };
            let msg = make_vfs_msg(vfs::VFS_WRITE, &[fd as u64, buf_ptr as u64, count as u64]);
            // A full pipe answers EAGAIN. Blocking fds must wait for the reader
            // to drain rather than surface it (mirroring the EAGAIN loop in
            // sys_read), or any pipeline carrying more than one ring's worth of
            // data dies with a spurious "failed to write whole buffer".
            let nonblock = vfs::fd_nonblock(pid, fd);
            // POSIX/Linux: a blocking pipe write of more than PIPE_BUF keeps
            // writing (waiting for ring space) until every byte is in. The
            // VFS arm accepts at most the free space per call; loop here.
            // A signal or EPIPE after partial progress returns the partial count.
            if !nonblock && count > vfs::PIPE_BUF && vfs::pipe_end_of(pid, fd).map_or(false, |(_, w)| w) {
                let mut done = 0usize;
                while done < count {
                    let rem = count - done;
                    prefault_user(buf_ptr + done, rem.min(vfs::PIPE_RING_SIZE));
                    let m = make_vfs_msg(vfs::VFS_WRITE, &[fd as u64, (buf_ptr + done) as u64, rem as u64]);
                    let n = block_until_ready(false,
                        || vfs_reply_val(&vfs::handle(&m, pid)),
                        || vfs_block_hint(pid, fd, true));
                    if n < 0 { return if done > 0 { done as isize } else { n }; }
                    if n == 0 { break; }
                    done += n as usize;
                }
                return done as isize;
            }
            block_until_ready(nonblock,
                || vfs_reply_val(&vfs::handle(&msg, pid)),
                || vfs_block_hint(pid, fd, true))
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
        // `/dev/tty0` is excluded: its read is the VT-change notification, not
        // console input (see vfs::fd_vt_number). `/dev/tty1`../dev/tty6` are
        // not — they read the console like any other console proxy. The extra
        // lookup only runs for fds fd_is_console_stdio already accepted.
        f if (f == 0 && !vfs::fd_redirected(current_pid(), 0))
            || (f < net_server::SOCK_FD_BASE
                && vfs::fd_is_console_stdio(current_pid(), f)
                && vfs::fd_vt_number(current_pid(), f) != Some(0)) => {
            if count == 0 { return 0; }
            if !is_kernel && !validate_user_buf(buf_ptr, count) { return -14; }
            // Job control: a background process of the console's session
            // reading its controlling terminal is stopped with SIGTTIN (EIO if
            // it cannot be) before any input is consumed. The stop runs
            // inside the check; it returns once the group is continued in
            // the foreground, or with the errno to report.
            if !is_kernel {
                let jc = tty_server::console_read_check();
                if jc != 0 { return jc; }
            }
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
                        if interrupted() { return if is_kernel { -4 } else { ERESTARTSYS }; }
                        if nonblocking {
                            spins += 1;
                            if spins >= NONBLOCK_RETRY_SPINS { return -11; } // EAGAIN
                            irq_window();
                            yield_now("sys_read_stdin"); // brief: don't sleep a NONBLOCK fd
                        } else {
                            // Blocking read: park on the poll wait-channel with a
                            // one-tick deadline instead of pegging a vCPU. Serial
                            // input is drained into evdev by the BSP tick, so the
                            // tick both wakes us and produces the byte; a virtio
                            // keypress wakes us early via wake_poll on its IRQ.
                            // This is what drops idle host CPU at the login prompt
                            // from a spun vCPU to ~100 Hz polling.
                            // Re-probe only *consumable* input (evdev queue +
                            // the synthetic-input ring) — NOT raw serial_has_data:
                            // a byte still in the UART isn't poppable until the
                            // tick drains it into evdev, and cancelling on it
                            // would tight-loop with IRQs masked so that drain
                            // tick could never fire. The now+1 deadline wakes us
                            // each tick to retry, which is exactly when the drain
                            // has run.
                            // Narrow keyed park (console_read_park): only console
                            // input, signals and the deadline reach it.
                            console_read_park(monotonic_ns() + CONSOLE_READ_BACKSTOP_NS);
                            if console_input_pending() || evdev_server::has_key_event(0)
                                || interrupted() {
                                sched::block_on_poll_cancel();
                            } else {
                                sched::block_on_poll_commit();
                            }
                        }
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
                // Fault-tolerant copy through the user mapping: a destination
                // page never touched yet (demand-zero .bss, or a .data page of
                // the exec image still absent) is faulted in like a user store
                // would. `write_user_buf` only fills pages already present and
                // answered EFAULT for those. No lock is held here.
                if !copy_to_user_bytes(buf_ptr, &kbuf[..n]) { return -14; }
            }

            n as isize
        }
        // read(2) on a socket ≡ recv(fd, buf, len, 0) — see the matching
        // socket route in sys_write. Blocking sockets loop on an empty ring
        // (std's exec-error socketpair is CLOEXEC but NOT nonblocking — the
        // parent's read must block until the child's exec/exit closes the
        // peer); O_NONBLOCK ones (mio/tokio) see EAGAIN immediately.
        f if is_sock_handle(f) => {
            if !is_kernel {
                if count != 0 && !validate_user_buf(buf_ptr, count) { return -14; }
                if count != 0 {
                    prefault_user(buf_ptr, count);
                }
            }
            let pid = current_pid();
            let msg = make_vfs_msg(net_server::NET_RECV,
                &[fd as u64, buf_ptr as u64, count as u64, 0, 0, 0]);
            net_block_lazy(pid, fd, false, false,
                || net_reply_val(&net_server::handle(&msg, pid)))
        }
        _ => {
            if !is_kernel {
                if count != 0 && !validate_user_buf(buf_ptr, count) { return -14; }
                if count != 0 && !user_buf_writable(buf_ptr, count) { return -14; }
                // Demand-page any not-yet-faulted pages in the destination buffer
                // so the VFS can copy directly without taking a kernel-mode fault.
                if count != 0 {
                    prefault_user(buf_ptr, pipe_prefault_len(current_pid(), fd, count, false));
                }
            }
            let pid = current_pid();
            let msg = make_vfs_msg(vfs::VFS_READ, &[fd as u64, buf_ptr as u64, count as u64]);
            // Pipe read: VFS returns -EAGAIN when write end is open but empty.
            // Block (park on the fd's wake tag) until data arrives or the write
            // end closes — but only for blocking fds. O_NONBLOCK readers (e.g.
            // an evdev poll loop) must see EAGAIN, or a single read() call
            // never returns while the device stays empty.
            let nonblock = vfs::fd_nonblock(pid, fd);
            block_until_ready(nonblock,
                || vfs_reply_val(&vfs::handle(&msg, pid)),
                || vfs_block_hint(pid, fd, false))
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
    let pid = current_pid();
    // The console fast path applies only to a stdio fd that has NOT been
    // redirected — the same rule sys_write follows. Without the fd_redirected
    // check a writev to a dup2'd fd 1 bypassed the pipe and went straight to
    // the UART; and the old `_ => -9` meant a writev to any other fd (a file, a
    // pipe, a socket) failed with EBADF instead of being written at all.
    let console = (matches!(fd, 1 | 2) && !vfs::fd_redirected(pid, fd))
        || (fd < net_server::SOCK_FD_BASE && vfs::fd_is_console_stdio(pid, fd));
    if console {
        // Same TOSTOP job-control gate as sys_write's console arm.
        let jc = tty_server::console_write_check();
        if jc != 0 { return jc; }
        // Gather every iovec into ONE kernel buffer, then hand the console a
        // single write. musl's stdio and tracing's fmt layer both emit a log
        // line as a multi-iovec writev; writing them one iovec at a time let
        // another vCPU splice its own output between an SGR prefix and the
        // text it colours, which is how spliced control sequences appeared in
        // the session log (see CONSOLE_OUT_LOCK). The copy is also what makes
        // holding that lock safe — the UART loop must never touch user memory.
        let mut total: isize = 0;
        let mut kbuf: Vec<u8> = Vec::new();
        for i in 0..iovcnt {
            let iov_addr = iov_ptr + i * 16;
            let [base, len] = match ua::read_user::<[usize; 2]>(iov_addr) { Some(v) => v, None => return -14 };
            if len == 0 { continue; }
            if !validate_user_buf(base, len) { return -14; }
            prefault_user(base, len);
            let off = kbuf.len();
            kbuf.resize(off + len, 0);
            let ok = with_current_address_space(|as_| {
                as_.read_user_buf(base, &mut kbuf[off..])
            }).unwrap_or(false);
            if !ok { return -14; }
            total = total.saturating_add(len as isize);
        }
        if !kbuf.is_empty() { console_write_user(kbuf.as_slice()); }
        return total;
    }
    // Everything else: one sys_write per iovec, which already routes sockets,
    // pipes and files correctly and blocks on a full pipe. On an O_APPEND
    // file the append lock is held across the whole vector, so it lands as
    // one contiguous append (it used to be one independent append per
    // iovec, and a concurrent appender could land between them).
    let _append = append_lock_for(pid, fd).map(|m| m.lock());
    let append_held = _append.is_some();
    let mut total: isize = 0;
    for i in 0..iovcnt {
        let iov_addr = iov_ptr + i * 16;
        let [base, len] = match ua::read_user::<[usize; 2]>(iov_addr) { Some(v) => v, None => return -14 };
        if len == 0 { continue; }
        let n = sys_write_impl(fd, base, len, append_held);
        if n < 0 { return if total > 0 { total } else { n }; }
        total = total.saturating_add(n);
        if (n as usize) < len { break; } // short write
    }
    total
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
                let [base, len] = match ua::read_user::<[usize; 2]>(iov_addr) { Some(v) => v, None => return -14 };
                if len > 0 { return sys_read(0, base, len); }
            }
            0
        }
        // A socket: VFS_READ answered EBADF for the whole socket fd range, so
        // readv(2) on a socket never worked. The first non-empty iovec is a
        // read(2) (blocking unless O_NONBLOCK); the rest only take what is
        // already queued, like one recvmsg over the whole vector.
        f if is_sock_handle(f) => {
            const MSG_DONTWAIT: usize = 0x40;
            let mut total: isize = 0;
            let mut first = true;
            for i in 0..iovcnt {
                let iov_addr = iov_ptr + i * 16;
                let [base, len] = match ua::read_user::<[usize; 2]>(iov_addr) { Some(v) => v, None => return -14 };
                if len == 0 { continue; }
                let n = if first { sys_read(fd, base, len) }
                        else { sys_recvfrom(fd, base, len, MSG_DONTWAIT, 0, 0) };
                first = false;
                if n < 0 { return if total > 0 { total } else { n }; }
                total = total.saturating_add(n);
                if (n as usize) < len { break; } // short read
            }
            total
        }
        _ => {
            let pid = current_pid();
            let mut total: isize = 0;
            for i in 0..iovcnt {
                let iov_addr = iov_ptr + i * 16;
                let [base, len] = match ua::read_user::<[usize; 2]>(iov_addr) { Some(v) => v, None => return -14 };
                if len == 0 { continue; }
                if !validate_user_buf(base, len) { return -14; }
                prefault_user(base, pipe_prefault_len(pid, fd, len, false));
                let msg = make_vfs_msg(vfs::VFS_READ, &[fd as u64, base as u64, len as u64]);
                // Blocking pipe: park on EAGAIN (EINTR = short read if partial).
                let nonblock = vfs::fd_nonblock(pid, fd);
                let n = block_until_ready(nonblock,
                    || vfs_reply_val(&vfs::handle(&msg, pid)),
                    || vfs_block_hint(pid, fd, false));
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
fn sys_tgkill(tgid: usize, tid: usize, sig: usize) -> isize {
    if sig > 64 { return -22; } // EINVAL
    if (tgid as i32) <= 0 || (tid as i32) <= 0 { return -22; } // EINVAL
    let (Ok(tgid), Ok(tid)) = (sched::pidns::from_cur(tgid as u32), sched::pidns::from_cur(tid as u32)) else { return -3 };
    sched::cred::kill_thread(Some(tgid), tid, sig as u32, tkill_info())
}

/// sys_tkill(tid, sig) — send a signal to a specific thread (legacy form of
/// tgkill without the thread-group-id argument). Used by raise()/pthread_kill.
fn sys_tkill(tid: usize, sig: usize) -> isize {
    if sig > 64 { return -22; } // EINVAL
    if (tid as i32) <= 0 { return -22; } // EINVAL
    let Ok(tid) = sched::pidns::from_cur(tid as u32) else { return -3 };
    sched::cred::kill_thread(None, tid, sig as u32, tkill_info())
}

/// rt_sigqueueinfo(tgid, sig, info) / rt_tgsigqueueinfo(tgid, tid, sig, info)
/// (`tgid_opt` set for the thread-directed form, whose target is `pid`). The
/// caller supplies the siginfo; Linux refuses (EPERM) a kernel- or kill-shaped
/// `si_code` (>= 0, or SI_TKILL) aimed at anyone but the caller itself, so a
/// process cannot forge a SI_USER/kernel origin. Then the kill permission
/// check. `si_pid`/`si_uid`/`si_value` travel to the handler as given.
fn sys_rt_sigqueueinfo(tgid_opt: Option<usize>, pid: usize, sig: usize, uinfo: usize) -> isize {
    if sig > 64 { return -22; } // EINVAL
    if !validate_user_buf(uinfo, 32) { return -14; } // EFAULT
    let mut raw = [0u8; 32];
    if unsafe { ua::copy_from_user(raw.as_mut_ptr(), uinfo, 32) } != 0 { return -14; }
    let rd32 = |o: usize| i32::from_le_bytes(raw[o..o + 4].try_into().unwrap());
    let code = rd32(8);
    let pid_i = pid as u32 as i32;
    if pid_i <= 0 { return if tgid_opt.is_some() { -22 } else { -3 }; } // EINVAL / ESRCH
    let me = sched::current_tgid();
    // Pid numbers are the caller's namespace view (lane nspid).
    let Ok(pid_k) = sched::pidns::from_cur(pid_i as u32) else { return -3 };
    let tgid_opt = match tgid_opt {
        Some(t) => { if (t as i32) <= 0 { return -22; } match sched::pidns::from_cur(t as u32) { Ok(k) => Some(k as usize), Err(e) => return e } }
        None => None,
    };
    let target_tgid = match tgid_opt {
        Some(t) => t as u32,
        None => sched::tgid_of(pid_k),
    };
    if (code >= 0 || code == sched::SI_TKILL) && target_tgid != me { return -1; } // EPERM
    let info = sched::SigInfo {
        si_code: code, si_pid: rd32(16), si_uid: rd32(20) as u32, si_status: 0, si_addr: 0,
        si_value: u64::from_le_bytes(raw[24..32].try_into().unwrap()),
    };
    match tgid_opt {
        Some(t) => sched::cred::kill_thread(Some(t as u32), pid_k, sig as u32, info),
        None => sched::cred::kill_process(target_tgid, sig as u32, info),
    }
}

/// `SI_TKILL`, not `SI_USER`: the two thread-directed forms are how `raise()`
/// and `pthread_kill()` reach the kernel, and Linux distinguishes them from
/// `kill(2)` for exactly that reason — a handler can tell a self-raise from an
/// externally delivered kill. `si_pid` is still the sending *process*.
fn tkill_info() -> sched::SigInfo {
    sched::SigInfo::tkill(sched::current_tgid(), sched::current_uid())
}

// ── Misc syscalls ─────────────────────────────────────────────────────────────

/// Host and NIS domain names (`uname` nodename/domainname), as (bytes, len)
/// without the NUL — the rest of each 65-byte field is zero-filled by uname.
static UTS_NAMES: spin::Mutex<(([u8; 64], usize), ([u8; 64], usize))> =
    spin::Mutex::new((uts_name(b"leandros"), uts_name(b"(none)")));

const fn uts_name(s: &[u8]) -> ([u8; 64], usize) {
    let mut b = [0u8; 64];
    let mut i = 0;
    while i < s.len() { b[i] = s[i]; i += 1; }
    (b, s.len())
}

/// sethostname(name, len) / setdomainname(name, len) (`domain`): CAP_SYS_ADMIN
/// (root) only — EPERM first, as on Linux — and at most 64 bytes (EINVAL).
fn sys_setxname(name_ptr: usize, len: usize, domain: bool) -> isize {
    if sched::current_euid() != 0 { return -1; } // EPERM
    if len > 64 { return -22; } // EINVAL
    if len > 0 && !validate_user_buf(name_ptr, len) { return -14; }
    let mut buf = [0u8; 64];
    if len > 0 {
        if unsafe { sched::uaccess::copy_from_user(buf.as_mut_ptr(), name_ptr, len) } != 0 { return -14; }
    }
    let mut names = UTS_NAMES.lock();
    if domain { names.1 = (buf, len); } else { names.0 = (buf, len); }
    0
}

/// sys_uname(buf) — return system identification.
///
/// Fills a Linux `struct utsname` (6 × 65-byte NUL-terminated fields).
fn sys_uname(buf_ptr_user: usize) -> isize {
    const UTSNAME_SIZE: usize = 6 * 65; // 390 bytes
    if !validate_user_buf(buf_ptr_user, UTSNAME_SIZE) { return -14; }
    // Built in a kernel buffer, copied out once (PAN-safe).
    let mut kb = [0u64; (UTSNAME_SIZE).div_ceil(8)];
    let buf_ptr = kb.as_mut_ptr() as usize;



    let names = UTS_NAMES.lock();
    // version: "#1 <build tag>" (crate::BUILD_TAG, set by build-all.sh), so
    // `uname -v` tells which kernel image is running.
    let mut version = [0u8; 65];
    let tag = crate::BUILD_TAG.as_bytes();
    version[..3].copy_from_slice(b"#1 ");
    let tn = tag.len().min(61);
    version[3..3 + tn].copy_from_slice(&tag[..tn]);
    let fields: [(&[u8], usize); 6] = [
        (b"Leandros\0",  0),    // sysname
        (&names.0 .0[..names.0 .1], 65),   // nodename (sethostname)
        (b"6.0.0-leandros\0", 130),  // release (= /proc/sys/kernel/osrelease)
        (&version[..3 + tn + 1], 195),  // version
        (#[cfg(target_arch = "aarch64")] b"aarch64\0",
         #[cfg(not(target_arch = "aarch64"))] b"x86_64\0",
         260),                // machine
        (&names.1 .0[..names.1 .1], 325),  // domainname (setdomainname)
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
    let rv = 0;
    if unsafe { ua::copy_to_user(buf_ptr_user, buf_ptr as *const u8, UTSNAME_SIZE) } != 0 { return -14; }
    rv
}

/// sys_getrlimit(resource, rlim_ptr) — return soft/hard limits.
///
/// All resources report RLIM_INFINITY (no real enforcement).
fn sys_getrlimit(resource: usize, rlim_ptr: usize) -> isize {
    sys_prlimit64(0, resource, 0, rlim_ptr)
}

/// The `struct rusage` fields this kernel accounts (the rest stay zero:
/// ixrss/idrss/isrss, nswap, inblock/oublock, msgsnd/msgrcv, nsignals).
struct RusageVals { cpu_ns: u64, sys_ns: u64, maxrss_kb: u64, minflt: u64, majflt: u64, nvcsw: u64, nivcsw: u64 }

impl RusageVals {
    fn of_child(a: &sched::ChildAcct) -> Self {
        RusageVals { cpu_ns: a.cpu_ns, sys_ns: a.sys_ns, maxrss_kb: a.maxrss_kb, minflt: a.minflt,
                     majflt: a.majflt, nvcsw: a.nvcsw, nivcsw: a.nivcsw }
    }
}

/// Store `v` as a 144-byte `struct rusage` at user address `ptr` (the same
/// layout on x86_64 and aarch64): ru_utime @0, ru_stime @16 (timevals; user
/// time is CPU time outside syscalls), ru_maxrss @32, ru_minflt @64,
/// ru_majflt @72, ru_nvcsw @128, ru_nivcsw @136. 0 or -EFAULT.
fn write_rusage(ptr: usize, v: &RusageVals) -> isize {
    let mut b = [0i64; 18];
    let user = v.cpu_ns.saturating_sub(v.sys_ns);
    b[0] = (user / 1_000_000_000) as i64;
    b[1] = ((user % 1_000_000_000) / 1000) as i64;
    b[2] = (v.sys_ns / 1_000_000_000) as i64;
    b[3] = ((v.sys_ns % 1_000_000_000) / 1000) as i64;
    b[4] = v.maxrss_kb as i64;
    b[8] = v.minflt as i64;
    b[9] = v.majflt as i64;
    b[16] = v.nvcsw as i64;
    b[17] = v.nivcsw as i64;
    if !validate_user_buf(ptr, 144) { return -14; }
    if unsafe { sched::uaccess::copy_to_user(ptr, b.as_ptr() as *const u8, 144) } != 0 { return -14; }
    0
}

/// sys_getrusage(who, usage_ptr) — resource usage of the calling process
/// (RUSAGE_SELF), its waited-for children (RUSAGE_CHILDREN) or the calling
/// thread (RUSAGE_THREAD).
///
/// CPU time is split at syscall boundaries (`Task::sys_ns`, `sched::sysacct`;
/// page faults and IRQs count as user time). minflt/majflt: the address
/// space's for the process, the thread's own for RUSAGE_THREAD (a fault is
/// major only when it read from a device). maxrss is the peak RSS (VmHWM),
/// carried across execve; for the thread, the process's, as on Linux.
fn sys_getrusage(who: usize, usage_ptr: usize) -> isize {
    const RUSAGE_SELF: isize = 0;
    const RUSAGE_THREAD: isize = 1;
    const RUSAGE_CHILDREN: isize = -1;
    let who = who as u32 as i32 as isize;
    let tgid = sched::current_tgid();
    let v = match who {
        RUSAGE_CHILDREN => RusageVals::of_child(&sched::process_child_acct(tgid)),
        RUSAGE_SELF => {
            let (ns, sys) = sched::process_cpu_sys_ns(tgid);
            let (minflt, majflt) = sched::process_faults(tgid);
            let (nvcsw, nivcsw) = sched::process_csw(tgid);
            RusageVals { cpu_ns: ns, sys_ns: sys, maxrss_kb: sched::process_maxrss_kb(tgid),
                         minflt, majflt, nvcsw, nivcsw }
        }
        RUSAGE_THREAD => {
            let (ns, sys) = sched::thread_cpu_sys_ns(current_pid());
            let (nvcsw, nivcsw, minflt, majflt) = sched::thread_rusage(current_pid()).unwrap_or((0, 0, 0, 0));
            RusageVals { cpu_ns: ns, sys_ns: sys, maxrss_kb: sched::process_maxrss_kb(tgid),
                         minflt, majflt, nvcsw, nivcsw }
        }
        _ => return -22,
    };
    write_rusage(usage_ptr, &v)
}

// ── Nice values (setpriority/getpriority) ────────────────────────────────────
//
// There is no `nice` syscall on either architecture we build for — it is
// i386-only — so nice(3) reaches the kernel as a getpriority/setpriority pair.

const PRIO_PROCESS: usize = 0;
const PRIO_PGRP:    usize = 1;
const PRIO_USER:    usize = 2;

/// Resolve a `(which, who)` pair, where `who == 0` means "the caller's own
/// process / process group / user".
/// PRIO_PROCESS / PRIO_PGRP `who` is a pid number in the caller's pid namespace.
fn nice_who(which: usize, who: usize) -> Result<usize, isize> {
    if (which == PRIO_PROCESS || which == PRIO_PGRP) && who != 0 {
        return sched::pidns::arg(who as u32).map(|k| k as usize);
    }
    Ok(who)
}

fn nice_target(which: usize, who: usize) -> Option<sched::NiceTarget> {
    let who = who as u32;
    match which {
        PRIO_PROCESS => Some(sched::NiceTarget::Process(
            if who == 0 { current_pid() } else { who })),
        PRIO_PGRP => Some(sched::NiceTarget::Pgrp(
            if who == 0 { sched::current_pgid() } else { who })),
        PRIO_USER => Some(sched::NiceTarget::User(
            if who == 0 { sched::current_euid() } else { who })),
        _ => None,
    }
}

/// setpriority(which, who, prio) — set the nice value of the selected tasks.
///
/// Unprivileged callers may only lower priority (raise the nice value) of
/// tasks they own; Linux gates the other direction on RLIMIT_NICE/CAP_SYS_NICE,
/// and root is exempt.
fn sys_setpriority(which: usize, who: usize, prio: usize) -> isize {
    let who = match nice_who(which, who) { Ok(w) => w, Err(e) => return e };
    let target = match nice_target(which, who) { Some(t) => t, None => return -22 };
    // `prio` arrives as a sign-extended int; clamping happens in the scheduler.
    let want = ((prio as u32) as i32).clamp(-20, 19) as i8;
    // Per task: EPERM unless the caller owns it (euid = its real or effective
    // uid) or is root; EACCES for lowering the nice value past RLIMIT_NICE
    // (0 by default, i.e. never) — Linux `set_one_prio`.
    sched::cred::set_nice_checked(|t| target.matches(t), want)
}

/// getpriority(which, who) — report the most favourable nice value in the set.
///
/// Returns `20 - nice`, not the nice value itself. That is the Linux ABI, and
/// it exists because a bare nice value spans -20..19 and would make a legal
/// result of -1 indistinguishable from an error return; the biased form is
/// 1..40, entirely positive. musl's `getpriority(3)` undoes the bias.
fn sys_getpriority(which: usize, who: usize) -> isize {
    let who = match nice_who(which, who) { Ok(w) => w, Err(e) => return e };
    let target = match nice_target(which, who) { Some(t) => t, None => return -22 };
    match sched::get_nice_for(target) {
        Some(nice) => 20 - nice as isize,
        None       => -3, // ESRCH
    }
}

/// sys_sched_getparam(pid, param_ptr) — fill sched_param with priority 0.
fn sys_sched_getparam(pid: usize, param_ptr: usize) -> isize {
    if let Err(e) = sched::pidns::arg(pid as u32) { return e; }
    // struct sched_param { int sched_priority; } = 4 bytes
    if param_ptr != 0 && validate_user_buf(param_ptr, 4) {
        unsafe { let _ = sched::uaccess::write_user::<i32>(param_ptr, 0); }
    }
    0
}

/// sys_sched_getaffinity(pid, cpusetsize, mask_ptr) — report every online CPU.
///
/// Rust's `std::thread::available_parallelism()` on linux-musl derives the
/// default Tokio/rayon worker-thread count directly from the popcount of
/// this mask, so under-reporting it (this used to hardcode "CPU 0 only")
/// silently pins every multi-threaded userland program — brush, cosmic-comp,
/// and anything else that sizes its thread pool this way — to a single
/// worker thread no matter how many vCPUs are actually online.
/// sched_setaffinity(2): the mask is stored on the thread and read back by
/// sched_getaffinity; EINVAL when it names no online CPU, as on Linux.
fn sys_sched_setaffinity(pid: usize, cpusetsize: usize, mask_ptr: usize) -> isize {
    let pid = match sched::pidns::arg(pid as u32) { Ok(k) => k as usize, Err(e) => return e };
    if (pid as isize) < 0 { return -22; }
    if mask_ptr == 0 { return -14; }
    let bytes = cpusetsize.min(8);
    if !validate_user_buf(mask_ptr, bytes) { return -14; }
    let mut m = [0u8; 8];
    if unsafe { ua::copy_from_user(m.as_mut_ptr(), mask_ptr, bytes) } != 0 { return -14; }
    let n = sched::active_cpu_count().min(64);
    let online = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    let mask = u64::from_le_bytes(m) & online;
    if mask == 0 { return -22; }
    let r = sched::cred::sched_setaffinity(pid as u32, Some(if mask == online { 0 } else { mask }));
    // Pinning ourselves off the CPU we run on: leave it now (the switch-back
    // hands us to an allowed CPU's queue) so the next instruction already
    // runs where the mask says.
    if r == 0 && (pid == 0 || pid as u32 == sched::current_pid()) {
        let mut tries = 0;
        while tries < 64 && (mask >> unsafe { sched::cpu_id() }.min(63)) & 1 == 0 && mask != online {
            yield_now("setaffinity");
            tries += 1;
        }
    }
    r
}

fn sys_sched_getaffinity(pid: usize, cpusetsize: usize, mask_ptr_user: usize) -> isize {
    let pid = match sched::pidns::arg(pid as u32) { Ok(k) => k as usize, Err(e) => return e };
    if (pid as isize) < 0 { return -22; }
    let stored = match sched::cred::sched_affinity_of(pid as u32) { Ok(m) => m, Err(e) => return e };
    if mask_ptr_user == 0 { return -14; }
    let bytes = cpusetsize.min(128);
    if !validate_user_buf(mask_ptr_user, bytes) { return -14; }
    // Built in a kernel buffer, copied out once (PAN-safe).
    let mut kb = [0u64; 16];
    let mask_ptr = kb.as_mut_ptr() as usize;

    // Set bits 0..n for the CPUs actually online, clamped to what the
    // caller's mask can hold (cpusetsize * 8 bits).
    let n = sched::active_cpu_count().min(bytes.saturating_mul(8));
    for cpu in 0..n {
        if stored != 0 && (cpu >= 64 || stored & (1 << cpu) == 0) { continue; }
        unsafe {
            let byte = mask_ptr.wrapping_add(cpu / 8) as *mut u8;
            *byte |= 1 << (cpu % 8);
        }
    }
    let rv = bytes as isize;
    if unsafe { ua::copy_to_user(mask_ptr_user, mask_ptr as *const u8, bytes) } != 0 { return -14; }
    rv
}

/// sys_getcpu(cpu_ptr, node_ptr, _tcache) — the CPU this thread is on (as

/// AT_HWCAP as Linux reports it. x86_64: CPUID.1:EDX. aarch64: the
/// `HWCAP_*` bits decoded from the ID registers, limited to features EL0 can
/// use as-is here (no SVE, pointer auth, CPUID emulation or event stream,
/// which need kernel support this kernel does not enable).
fn linux_hwcap() -> u64 {
    #[cfg(target_arch = "x86_64")]
    { unsafe { core::arch::x86_64::__cpuid(1).edx as u64 } }
    #[cfg(target_arch = "aarch64")]
    {
        let (isar0, isar1, pfr0): (u64, u64, u64);
        unsafe {
            core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack));
            core::arch::asm!("mrs {}, id_aa64isar1_el1", out(reg) isar1, options(nomem, nostack));
            core::arch::asm!("mrs {}, id_aa64pfr0_el1", out(reg) pfr0, options(nomem, nostack));
        }
        let f = |r: u64, sh: u32| (r >> sh) & 0xf;
        let mut h = 0u64;
        let fp = f(pfr0, 16);
        let simd = f(pfr0, 20);
        if fp != 0xf { h |= 1 << 0; if fp >= 1 { h |= 1 << 9; } }          // FP, FPHP
        if simd != 0xf { h |= 1 << 1; if simd >= 1 { h |= 1 << 10; } }     // ASIMD, ASIMDHP
        if f(isar0, 4) >= 1 { h |= 1 << 3; }                               // AES
        if f(isar0, 4) >= 2 { h |= 1 << 4; }                               // PMULL
        if f(isar0, 8) >= 1 { h |= 1 << 5; }                               // SHA1
        if f(isar0, 12) >= 1 { h |= 1 << 6; }                              // SHA2
        if f(isar0, 12) >= 2 { h |= 1 << 21; }                             // SHA512
        if f(isar0, 16) >= 1 { h |= 1 << 7; }                              // CRC32
        if f(isar0, 20) >= 2 { h |= 1 << 8; }                              // ATOMICS
        if f(isar0, 28) >= 1 { h |= 1 << 12; }                             // ASIMDRDM
        if f(isar0, 32) >= 1 { h |= 1 << 17; }                             // SHA3
        if f(isar0, 36) >= 1 { h |= 1 << 18; }                             // SM3
        if f(isar0, 40) >= 1 { h |= 1 << 19; }                             // SM4
        if f(isar0, 44) >= 1 { h |= 1 << 20; }                             // ASIMDDP
        if f(isar0, 48) >= 1 { h |= 1 << 23; }                             // ASIMDFHM
        if f(isar0, 52) >= 1 { h |= 1 << 27; }                             // FLAGM
        if f(isar1, 0) >= 1 { h |= 1 << 16; }                              // DCPOP
        if f(isar1, 12) >= 1 { h |= 1 << 13; }                             // JSCVT
        if f(isar1, 16) >= 1 { h |= 1 << 14; }                             // FCMA
        if f(isar1, 20) >= 1 { h |= 1 << 15; }                             // LRCPC
        if f(isar1, 20) >= 2 { h |= 1 << 26; }                             // ILRCPC
        if f(isar1, 36) >= 1 { h |= 1 << 29; }                             // SB
        h
    }
}

/// on Linux, possibly stale by the time the caller looks), NUMA node 0.
fn sys_getcpu(cpu_ptr: usize, node_ptr: usize, _tcache: usize) -> isize {
    if cpu_ptr != 0 {
        if !validate_user_buf(cpu_ptr, 4) { return -14; }
        let cpu = unsafe { sched::cpu_id() } as u32;
        unsafe { let _ = sched::uaccess::write_user::<u32>(cpu_ptr, cpu); }
    }
    if node_ptr != 0 {
        if !validate_user_buf(node_ptr, 4) { return -14; }
        unsafe { let _ = sched::uaccess::write_user::<u32>(node_ptr, 0); }
    }
    0
}

// ── NUMA memory policy (single node) ─────────────────────────────────────────
// Linux with CONFIG_NUMA on a one-node machine: every policy is accepted (all
// memory comes from node 0 anyway), get_mempolicy reports MPOL_DEFAULT and an
// empty mask, or node 0 for MPOL_F_NODE / MPOL_F_MEMS_ALLOWED. FFmpeg/libnuma-
// style probes (seen from Firefox's media process) then work as on Linux.
const MPOL_MAX: usize = 7; // DEFAULT..PREFERRED_MANY
const MPOL_F_NODE: usize = 1;
const MPOL_F_ADDR: usize = 2;
const MPOL_F_MEMS_ALLOWED: usize = 4;
const MPOL_MODE_FLAGS: usize = (1 << 15) | (1 << 14) | (1 << 13); // STATIC|RELATIVE|NUMA_BALANCING

/// Write a nodemask of `maxnode` bits (rounded up to longs) with only `node0`.
fn write_nodemask(nmask: usize, maxnode: usize, node0: bool) -> isize {
    if nmask == 0 { return 0; }
    let longs = maxnode.div_ceil(64);
    if longs == 0 { return 0; }
    if !validate_user_buf(nmask, longs * 8) { return -14; }
    for i in 0..longs {
        let v: u64 = if i == 0 && node0 { 1 } else { 0 };
        if !ua::write_user::<u64>(nmask + i * 8, v) { return -14; }
    }
    0
}

/// Read a user nodemask: Ok(true) if it names node 0, Ok(false) if empty,
/// EINVAL if it names any other node (none exist).
fn read_nodemask_has_node0(nmask: usize, maxnode: usize) -> Result<bool, isize> {
    if nmask == 0 || maxnode == 0 { return Ok(false); }
    let longs = maxnode.div_ceil(64);
    if !validate_user_buf(nmask, longs * 8) { return Err(-14); }
    let (mut has0, mut other) = (false, false);
    for i in 0..longs {
        let mut v = ur::<u64>(nmask + i * 8);
        // Bits at and above maxnode are ignored, as on Linux.
        let keep = maxnode - i * 64;
        if keep < 64 { v &= (1u64 << keep) - 1; }
        if i == 0 { has0 = v & 1 != 0; v &= !1; }
        if v != 0 { other = true; }
    }
    if other { Err(-22) } else { Ok(has0) }
}

fn sys_get_mempolicy(mode_ptr: usize, nmask: usize, maxnode: usize, addr: usize, flags: usize) -> isize {
    if flags & !(MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED) != 0 { return -22; }
    if flags & MPOL_F_MEMS_ALLOWED != 0 && flags & (MPOL_F_NODE | MPOL_F_ADDR) != 0 { return -22; }
    if nmask != 0 && maxnode < 1 { return -22; }
    if flags & MPOL_F_ADDR == 0 && addr != 0 { return -22; }
    let mode: i32 = 0; // MPOL_DEFAULT, or node 0 for MPOL_F_NODE
    if mode_ptr != 0 {
        if !validate_user_buf(mode_ptr, 4) { return -14; }
        uw::<i32>(mode_ptr, mode);
    }
    // MEMS_ALLOWED reports the allowed set (node 0); the default policy's own
    // mask is empty.
    write_nodemask(nmask, maxnode, flags & MPOL_F_MEMS_ALLOWED != 0)
}

fn sys_set_mempolicy(mode: usize, nmask: usize, maxnode: usize) -> isize {
    let m = mode & !MPOL_MODE_FLAGS;
    if m >= MPOL_MAX { return -22; }
    match read_nodemask_has_node0(nmask, maxnode) {
        Err(e) => e,
        // DEFAULT takes no nodes; BIND/INTERLEAVE/PREFERRED_MANY need some
        // (node 0); PREFERRED (1) and LOCAL (4) accept an empty mask.
        Ok(has0) => match m {
            0 => if has0 { -22 } else { 0 },
            2 | 3 | 5 | 6 => if has0 { 0 } else { -22 },
            _ => 0,
        },
    }
}

fn sys_mbind(addr: usize, len: usize, mode: usize, nmask: usize, maxnode: usize, _flags: usize) -> isize {
    if addr & 0xFFF != 0 { return -22; }
    let _ = len;
    sys_set_mempolicy(mode, nmask, maxnode)
}

/// sys_capget(hdr_ptr, data_ptr) — return empty capability sets (running as root).
/// Validate a `__user_cap_header_struct` and return how many 12-byte
/// `__user_cap_data_struct`s its version uses (v1: one, v2/v3: two). An
/// unknown version is answered Linux's way: the preferred version (v3) is
/// written back and the call fails EINVAL.
fn cap_header_words(hdr_ptr: usize) -> Result<(usize, i32), isize> {
    const V1: u32 = 0x1998_0330; const V2: u32 = 0x2007_1026; const V3: u32 = 0x2008_0522;
    if !validate_user_buf(hdr_ptr, 8) { return Err(-14); }
    let (ver, pid) = match ua::read_user::<[u32; 2]>(hdr_ptr) { Some([v, p]) => (v, p as i32), None => return Err(-14) };
    match ver {
        V1 => Ok((1, pid)),
        V2 | V3 => Ok((2, pid)),
        _ => { if !ua::write_user::<u32>(hdr_ptr, V3) { return Err(-14); } Err(-22) }
    }
}

/// capget(hdr, data): the target thread's effective / permitted /
/// inheritable sets (see `sched::caps`).
fn sys_capget(hdr_ptr: usize, data_ptr: usize) -> isize {
    let (words, pid) = match cap_header_words(hdr_ptr) {
        Ok(v) => v,
        Err(-22) if data_ptr == 0 => return 0, // version probe
        Err(e) => return e,
    };
    if pid < 0 { return -22; }
    let pid = match sched::pidns::arg(pid as u32) { Ok(k) => k as i32, Err(e) => return e };
    if pid != 0 && sched::exists_probe(pid as u32) != 0 { return -3; } // ESRCH
    let c = match sched::caps::caps_of(pid as u32) { Some(c) => c, None => return -3 };
    if data_ptr == 0 { return 0; }
    if !validate_user_buf(data_ptr, 12 * words) { return -14; }
    let w = |v: u64, hi: bool| if hi { (v >> 32) as u32 } else { v as u32 };
    // Per word i: eff, perm, inh (struct __user_cap_data_struct[i]).
    for i in 0..words {
        for (j, v) in [c[0], c[1], c[2]].iter().enumerate() {
            if !ua::write_user::<u32>(data_ptr + 4 * (3 * i + j), w(*v, i == 1)) { return -14; }
        }
    }
    0
}

/// capset(hdr, data): only the caller's own sets (pid 0 or self, else EPERM);
/// Linux rules in `sched::caps::set_caps`.
fn sys_capset(hdr_ptr: usize, data_ptr: usize) -> isize {
    let (words, pid) = match cap_header_words(hdr_ptr) { Ok(v) => v, Err(e) => return e };
    if pid != 0 && pid as u32 != sched::current_tgid() && pid as u32 != current_pid() { return -1; }
    if !validate_user_buf(data_ptr, 12 * words) { return -14; }
    let mut v = [[0u32; 2]; 3]; // [eff, prm, inh][word]
    for i in 0..words {
        for j in 0..3 {
            v[j][i] = match ua::read_user::<u32>(data_ptr + 12 * i + 4 * j) { Some(x) => x, None => return -14 };
        }
    }
    let q = |a: [u32; 2]| a[0] as u64 | (a[1] as u64) << 32;
    sched::caps::set_caps(q(v[0]), q(v[1]), q(v[2]))
}

/// sys_statx(dirfd, path, flags, mask, statxbuf) — extended stat.
///
/// Delegates to sys_newfstatat for the path lookup, then zero-extends to the
/// wider statx layout.  The statx struct is 256 bytes; struct stat is 144 bytes.
fn sys_statx(dirfd: usize, path_ptr: usize, flags: usize, _mask: usize, statxbuf_user: usize) -> isize {
    if !validate_user_buf(statxbuf_user, 256) { return -14; }
    // Built in a kernel buffer, copied out once (PAN-safe).
    let mut kb = [0u64; 32];
    let statxbuf = kb.as_mut_ptr() as usize;
    // Zero the entire statx buffer first.

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
    //        stx_blocks(u64@48), stx_attributes_mask(u64@56), stx_atime(@64),
    //        stx_btime(@80), stx_ctime(@96), stx_mtime(@112) as {i64 sec, u32 nsec, pad},
    //        stx_rdev_major/minor(u32@128/132), stx_dev_major/minor(u32@136/140).
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
        core::ptr::write_unaligned((statxbuf) as *mut u32, 0x7ff);
        // stx_blksize
        core::ptr::write_unaligned((statxbuf +  4) as *mut u32, blksize);
        // stx_nlink — the real link count, not a hardcoded 1
        core::ptr::write_unaligned((statxbuf + 16) as *mut u32, nlink);
        // stx_mode
        core::ptr::write_unaligned((statxbuf + 28) as *mut u16, mode as u16);
        // stx_ino
        core::ptr::write_unaligned((statxbuf + 32) as *mut u64, ino);
        // stx_size
        core::ptr::write_unaligned((statxbuf + 40) as *mut i64, size);
        // stx_blocks
        core::ptr::write_unaligned((statxbuf + 48) as *mut i64, blocks);
        // stx_uid / stx_gid. Never filled before while stx_mask claimed
        // them: every statx caller saw every file as root's. Bun checks
        // that its private temp directory is its own, and the Claude Code
        // CLI run as a regular user refused /tmp/claude-1000 ("owned by
        // uid 0, expected 1000").
        let (uid, gid) = vfs::read_stat_owner(stat_ptr);
        core::ptr::write_unaligned((statxbuf + 20) as *mut u32, uid);
        core::ptr::write_unaligned((statxbuf + 24) as *mut u32, gid);
        // Timestamps: struct stat keeps (sec, nsec) pairs at 72/88/104 for
        // atime/mtime/ctime on both ABIs; statx_timestamp is {i64 sec, u32
        // nsec, i32 pad} at 64 (atime), 96 (ctime) and 112 (mtime). btime is
        // not known and not claimed in stx_mask.
        for (src, dst) in [(72usize, 64usize), (104, 96), (88, 112)] {
            let sec  = ((stat_ptr + src) as *const i64).read_unaligned();
            let nsec = ((stat_ptr + src + 8) as *const i64).read_unaligned();
            core::ptr::write_unaligned((statxbuf + dst) as *mut i64, sec);
            core::ptr::write_unaligned((statxbuf + dst + 8) as *mut u32, nsec as u32);
        }
        // stx_rdev_* / stx_dev_*: dev_t split the way glibc's major()/minor() do.
        #[cfg(target_arch = "x86_64")]
        const ST_RDEV_OFF: usize = 40;
        #[cfg(target_arch = "aarch64")]
        const ST_RDEV_OFF: usize = 32;
        let split = |d: u64| -> (u32, u32) {
            ((((d >> 8) & 0xfff) | ((d >> 32) & !0xfff)) as u32,
             ((d & 0xff) | ((d >> 12) & !0xff)) as u32)
        };
        let (rmaj, rmin) = split(((stat_ptr + ST_RDEV_OFF) as *const u64).read_unaligned());
        let (dmaj, dmin) = split((stat_ptr as *const u64).read_unaligned());
        core::ptr::write_unaligned((statxbuf + 128) as *mut u32, rmaj);
        core::ptr::write_unaligned((statxbuf + 132) as *mut u32, rmin);
        core::ptr::write_unaligned((statxbuf + 136) as *mut u32, dmaj);
        core::ptr::write_unaligned((statxbuf + 140) as *mut u32, dmin);
    }
    let rv = 0;
    if unsafe { ua::copy_to_user(statxbuf_user, statxbuf as *const u8, 256) } != 0 { return -14; }
    rv
}

/// Give the calling thread a private copy of the fd table it shares (Linux
/// `unshare_fd`). A sole user of its table has nothing to split.
fn fd_private() -> bool {
    sched::fdkey::any() && sched::fdkey::override_of(current_pid()).is_some()
}

fn fd_unshare_current() -> isize {
    const F_GETFD: usize = 1;
    let tid = current_pid();
    let tgid = sched::current_tgid();
    let old = vfs::tbl_key(tgid);
    if sched::fdkey::users(old, tgid) <= 1 { return 0; }
    let new = sched::fdkey::alloc_key();
    if !vfs::copy_fd_table(tgid, new, tgid) { return 0; }
    if !sched::fdkey::set(tid, new, tgid) {
        let m = make_vfs_msg(vfs::VFS_CLOSE_ALL, &[new as u64]);
        let _ = vfs::handle(&m, tid);
        return -12;
    }
    // A socket or epoll handle is owned by exactly one descriptor (dup makes
    // a new handle), so the copy must not share handle numbers with the old
    // table.
    for (fd, h) in vfs::handle_fds(tid) {
        let cloexec = sys_fcntl(h, F_GETFD, 0) == 1;
        let d = dup_handle(tid, h, cloexec);
        if d >= 0 { let _ = vfs::install_handle_at(tid, fd, d as usize); }
        else { let m = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]); let _ = vfs::handle(&m, tid); }
    }
    0
}

/// sys_unshare(flags). CLONE_FS: the calling thread stops sharing cwd, root
/// and umask with the threads (and CLONE_FS children) it shared them with.
/// CLONE_SYSVSEM is accepted for a single-threaded caller, for which it
/// changes nothing. CLONE_FILES gives the calling thread a private copy of
/// the fd table it shares (`fd_unshare_current`). Namespaces are EINVAL (none
/// are implemented), unknown bits too.
fn sys_unshare(flags: usize) -> isize {
    let flags = flags as u32 as usize;
    let known = CLONE_FS | CLONE_FILES | CLONE_SYSVSEM | CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS
        | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET | CLONE_THREAD | CLONE_SIGHAND | CLONE_VM;
    if flags & !known != 0 { return -22; }
    let alone = sched::thread_count(sched::current_tgid()) <= 1;
    // CLONE_THREAD/SIGHAND/VM: Linux allows them only for a single-threaded
    // caller, where they are no-ops.
    if flags & (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) != 0 && !alone { return -22; }
    if flags & (CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS) != 0 { return -22; }
    let flat = flags & (CLONE_NEWNET | CLONE_NEWIPC);
    if flat != 0 && flags & CLONE_NEWUSER == 0 && !sched::caps::ns_capable(21) { return -1; } // CAP_SYS_ADMIN
    if flat != 0 && !alone { return -22; }
    if flags & CLONE_NEWUSER != 0 {
        // Linux: a chrooted task may not create a user namespace (EPERM).
        if sched::current_root_arc().is_some() { return -1; }
        // Linux: a multithreaded caller cannot change user namespace.
        if !alone { return -22; }
        let r = sched::ns::unshare_user();
        if r < 0 { return r; }
        sched::unshare_fs(); // CLONE_NEWUSER implies CLONE_FS
    }
    // Run after the NEWUSER block: NEWUSER|NEWPID gets its caps from the new userns.
    if flags & CLONE_NEWPID != 0 {
        let r = sched::pidns::unshare_pid();
        if r < 0 { return r; }
    }
    if flat != 0 {
        let r = sched::ns::unshare_flat(flat & CLONE_NEWNET != 0, flat & CLONE_NEWIPC != 0);
        if r < 0 { return r; }
    }
    if flags & CLONE_FILES != 0 { let r = fd_unshare_current(); if r < 0 { return r; } }
    if flags & CLONE_FS != 0 { sched::unshare_fs(); }
    0
}

/// sys_close_range(first, last, flags) — close (or, with CLOSE_RANGE_CLOEXEC,
/// mark close-on-exec) every open fd in `first..=last`: VFS fds, sockets and
/// epoll fds alike — each kind lives in its own number range here, and only
/// the VFS one used to be swept.
///
/// CLOSE_RANGE_UNSHARE first gives the caller a private copy of an fd table
/// it shares (`fd_unshare_current`); for a single-threaded caller (a child
/// between fork and exec) that is a no-op.
fn sys_close_range(first: usize, last: usize, flags: usize) -> isize {
    const CLOSE_RANGE_UNSHARE: usize = 1 << 1;
    const CLOSE_RANGE_CLOEXEC: usize = 1 << 2;
    const F_SETFD: usize = 2;
    const FD_CLOEXEC: usize = 1;
    // `unsigned int` arguments.
    let (first, last, flags) = (first as u32 as usize, last as u32 as usize, flags as u32 as usize);
    if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 || first > last { return -22; }
    if flags & CLOSE_RANGE_UNSHARE != 0 { let r = fd_unshare_current(); if r < 0 { return r; } }
    let cloexec = flags & CLOSE_RANGE_CLOEXEC != 0;
    let tgid = sched::current_tgid();
    let end = last.min(vfs::FD_NUMBER_CEIL - 1);
    if first > end { return 0; }
    // Number-keyed registrations first, while the numbers still name the
    // files being closed (see `epoll_release_fd`); description-keyed ones go
    // with the description's last reference.
    if !cloexec && !fd_private() { epoll_drop_range(tgid, first, end); }
    // One table holds every kind of descriptor (sockets and epoll fds are
    // entries in it), so one listing covers them all.
    let open: alloc::vec::Vec<usize> = vfs::open_fds(tgid).into_iter()
        .filter(|&fd| (first..=end).contains(&fd)).collect();
    for fd in open {
        // sys_fcntl is called directly, past `fd_handle_args`: a socket's or
        // an epoll fd's close-on-exec flag lives on its handle.
        if cloexec { let _ = sys_fcntl(fd_handle(fd), F_SETFD, FD_CLOEXEC); } else { let _ = sys_close(fd); }
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

/// sys_prlimit64(pid, resource, new_limit, old_limit) — also getrlimit and
/// setrlimit (pid 0). Limits are stored per process (`Task::rlimits`) and
/// mostly not enforced — RLIMIT_NICE and RLIMIT_RTPRIO gate setpriority and
/// sched_setscheduler, RLIMIT_NOFILE bounds every descriptor number (the fd
/// tables hold a copy of its soft limit) — but the rules are Linux's: cur >
/// max is EINVAL, raising a hard limit needs root, a NOFILE hard limit past
/// nr_open is EPERM, and another process's limits need the same uids and
/// gids or root (see `sched::cred::prlimit`).
fn sys_prlimit64(
    pid:      usize,
    res:      usize,
    new_ptr:  usize,
    old_ptr:  usize,
) -> isize {
    // struct rlimit64 { rlim_cur: u64, rlim_max: u64 } = 16 bytes
    if (pid as i32) < 0 { return -3; } // ESRCH
    let pid = match sched::pidns::arg(pid as u32) { Ok(k) => k as usize, Err(e) => return e };
    let new = if new_ptr != 0 {
        if !validate_user_buf(new_ptr, 16) { return -14; }
        match ua::read_user::<[u64; 2]>(new_ptr) { Some(v) => Some(v), None => return -14 }
    } else { None };
    if old_ptr != 0 && !validate_user_buf(old_ptr, 16) { return -14; }
    match sched::cred::prlimit(pid as u32, res, new) {
        Ok(old) => {
            // RLIMIT_NOFILE is enforced by the fd tables, which keep their own
            // copy of the soft limit (no scheduler call under FD_TABLES).
            if let (Some([cur, _]), sched::task::RLIMIT_NOFILE) = (new, res) {
                let tgid = if pid == 0 { sched::current_tgid() } else { sched::tgid_of(pid as u32) };
                vfs::set_fd_limit(tgid, cur.min(sched::task::NR_OPEN) as usize);
            }
            if old_ptr != 0 && !ua::write_user::<[u64; 2]>(old_ptr, [old[0], old[1]]) { return -14; }
            0
        }
        Err(e) => e,
    }
}

/// sched_setscheduler(pid, policy, param) and sched_setparam(pid, param)
/// (`policy == None`). Admission only — see `sched::cred::sched_policy_check`.
fn sys_sched_setscheduler(pid: usize, policy: Option<usize>, param_ptr: usize) -> isize {
    let pid = match sched::pidns::arg(pid as u32) { Ok(k) => k as usize, Err(e) => return e };
    const SCHED_RESET_ON_FORK: usize = 0x4000_0000;
    if (pid as i32) < 0 || param_ptr == 0 { return -22; } // EINVAL
    if !validate_user_buf(param_ptr, 4) { return -14; }
    let prio = match ua::read_user::<i32>(param_ptr) { Some(v) => v, None => return -14 };
    let policy = policy.map(|p| ((p as u32 as i32) as usize & !SCHED_RESET_ON_FORK) as u32);
    if let Some(p) = policy { if (p as i32) < 0 { return -22; } }
    sched::cred::sched_policy_check(pid as u32, policy, prio)
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
    let msg = make_vfs_msg(vfs::VFS_OPEN, &[path_ptr as u64, flags as u64,
                                            pack_create_mode(mode)]);
    let fd = vfs_reply_val(&vfs::handle(&msg, pid));
    // open(O_TRUNC) of a mapped f2fs file empties it like ftruncate(0), so
    // its mappings lose every page as there (tmpfs does this in the VFS).
    const O_TRUNC: usize = 0o1000;
    if fd >= 0 && flags & O_TRUNC != 0 {
        if let Some(vfs::VnodeKind::MountedFile { port, file_id }) = vfs::vfs_get_node_kind(pid, fd as usize) {
            if let Some(cap) = f2fs_server::inode_by_port(port, file_id as u64)
                .and_then(|ino| mmap_cap_get(port, ino))
            {
                sched::zap_backing_all(cap, 0, 0, u64::MAX);
                mmap_file_ref(cap, false);
            }
        }
    }
    fd
}

/// Pack a creation mode with the caller's umask for the filesystem
/// (`xattr::pack_create_mode`).
///
/// The umask used to be applied here, in the kernel, so tmpfs and f2fs could
/// not disagree about the mode of a new file. It still cannot: both now run
/// `xattr::acl_create`, which applies the umask itself — unless the parent
/// directory carries a default POSIX ACL, in which case the ACL, not the
/// umask, decides the child's permission bits (POSIX 1003.1e; Linux
/// `posix_acl_create`). Only the filesystem knows which case it is, so the
/// mask has to travel with the mode. `u32::MAX` reads the mask unchanged.
fn pack_create_mode(mode: usize) -> u64 {
    xattr::pack_create_mode(mode as u32, sched::umask(u32::MAX))
}

/// Open a kernel-resident path string (no user-pointer validation, no cwd
/// resolution — callers inside the kernel already hold an absolute path).
///
/// This exists because `sys_open` now funnels through `resolve_user_path`,
/// whose `validate_user_buf` check rejects any address at or above the
/// user/kernel split.  Kernel callers that used to hand `sys_open` a heap
/// pointer silently started getting `-EFAULT`.
fn open_kernel_path(path: &str, flags: usize, mode: usize) -> isize {
    if path.len() >= HOST_PATH_MAX { return -36; } // ENAMETOOLONG
    let kp = KPath::raw(path.as_bytes());
    vfs_open_resolved(kp.ptr(), flags, mode)
}

/// The user's open(2) flags in the layout the VFS and the filesystems use,
/// which is x86_64's. On aarch64 (asm-generic) four bits differ:
///
/// | flag        | x86_64  | aarch64 |
/// |-------------|---------|---------|
/// | O_DIRECT    | 0o40000 | 0o200000|
/// | O_LARGEFILE | 0o100000| 0o400000|
/// | O_DIRECTORY | 0o200000| 0o40000 |
/// | O_NOFOLLOW  | 0o400000| 0o100000|
///
/// Untranslated, aarch64's O_LARGEFILE — which musl ORs into EVERY open() —
/// read as O_NOFOLLOW, so every open through a symlink failed ELOOP (cat,
/// musl's ldso loading libz.so.1 -> libz.so.1.3.2, ...), and O_DIRECTORY /
/// O_NOFOLLOW themselves were not enforced.
/// The four differing bits, as `(aarch64, x86_64)` pairs.
#[cfg(target_arch = "aarch64")]
const OPEN_FLAG_REMAP: [(usize, usize); 4] = [
    (0o200000, 0o40000),  // O_DIRECT
    (0o400000, 0o100000), // O_LARGEFILE
    (0o40000,  0o200000), // O_DIRECTORY
    (0o100000, 0o400000), // O_NOFOLLOW
];

#[inline]
fn open_flags_from_user(flags: usize) -> usize {
    #[cfg(target_arch = "aarch64")]
    {
        let mut f = flags;
        for &(a, _) in &OPEN_FLAG_REMAP { f &= !a; }
        for &(a, x) in &OPEN_FLAG_REMAP { if flags & a != 0 { f |= x; } }
        f
    }
    #[cfg(not(target_arch = "aarch64"))]
    { flags }
}

/// The inverse of [`open_flags_from_user`], for status flags handed back to
/// user space (`fcntl(F_GETFL)`): the VFS stores them in the x86_64 layout,
/// and an aarch64 caller that reads O_LARGEFILE back as O_NOFOLLOW (and
/// feeds it to F_SETFL) must see its own layout.
#[inline]
fn open_flags_to_user(flags: usize) -> usize {
    #[cfg(target_arch = "aarch64")]
    {
        let mut f = flags;
        for &(_, x) in &OPEN_FLAG_REMAP { f &= !x; }
        for &(a, x) in &OPEN_FLAG_REMAP { if flags & x != 0 { f |= a; } }
        f
    }
    #[cfg(not(target_arch = "aarch64"))]
    { flags }
}

fn sys_open(path_ptr: usize, flags: usize, mode: usize) -> isize {
    sys_openat(AT_FDCWD, path_ptr, flags, mode)
}

fn sys_openat(dirfd: usize, path_ptr: usize, flags: usize, mode: usize) -> isize {
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let flags = open_flags_from_user(flags);
    let path = if flags & OPEN_O_NOFOLLOW == 0 {
        match follow_magic_final(path) { Ok(p) => p, Err(e) => return e }
    } else { path };
    vfs_open_resolved(path.ptr(), flags, mode)
}

/// `lstat` of `/proc/<pid>/{exe,cwd,root}` or `/proc/<pid>/fd/<n>`: the link
/// itself, `S_IFLNK`, owned by the process's effective ids (Linux proc
/// inodes; exe/cwd/root are 0777, fd links 0700), size 0. `None` when `ns`
/// is no magic link; ENOENT when the process (or the descriptor) is gone.
fn lstat_magic_link(ns: &[u8], statbuf_ptr: usize) -> Option<isize> {
    if let Some((pid, k)) = proc_ns_link(ns) {
        if sched::exists_probe(pid) != 0 { return Some(-2); }
        if !proc_link_allowed(pid) { return Some(-13); }
        let (u, g) = sched::cred::ids_of(pid).map(|(u, g)| (u[1], g[1])).unwrap_or((0, 0));
        vfs::write_stat_full(statbuf_ptr, 0o120777, 1, 0, 0x7200_0000_0000u64 | ((pid as u64) << 8) | k as u64, u, g);
        return Some(0);
    }
    if !is_magic_link(ns) { return None; }
    let mut denied = false;
    let walked = backing_walk_d(ns, &mut false, &mut denied);
    if denied { return Some(-13); }
    let b = walked.unwrap_or_else(|| ns.to_vec());
    let mut found = None;
    for (k, what) in [&b"exe"[..], b"cwd", b"root", b"fd"].iter().enumerate() {
        if let Some((pid, rest)) = proc_magic(&b, what) { found = Some((k, pid, rest.to_vec())); break; }
    }
    let (k, pid, rest) = found?;
    if sched::exists_probe(pid) != 0 { return Some(-2); }
    if k != 3 && !proc_link_allowed(pid) { return Some(-13); }
    let mut mode = 0o120777;
    let mut ino = 0x7000_0000_0000u64 | ((pid as u64) << 8) | k as u64;
    if k == 3 {
        let mut fd = 0usize;
        for &d in rest.strip_prefix(b"/").unwrap_or(b"") { fd = fd * 10 + (d - b'0') as usize; }
        let tg = sched::tgid_of(pid);
        if !vfs::fd_is_console_stdio(tg, fd) && vfs::fd_flags(tg, fd).is_none() { return Some(-2); }
        mode = 0o120700;
        ino = 0x7100_0000_0000u64 | ((pid as u64) << 20) | fd as u64;
    }
    let (u, g) = sched::cred::ids_of(pid).map(|(u, g)| (u[1], g[1])).unwrap_or((0, 0));
    vfs::write_stat_full(statbuf_ptr, mode, 1, 0, ino, u, g);
    Some(0)
}

/// O_NOFOLLOW in the VFS (x86_64) open-flag layout.
const OPEN_O_NOFOLLOW: usize = 0o400000;

/// Follow a final `/proc/<pid>/{exe,cwd,root}` magic link, as open and stat
/// do on Linux: `open("/proc/self/exe")` opens the running binary,
/// `stat("/proc/self/cwd")` the working directory. Anything else (including
/// `/proc/<pid>/fd/N`, which the VFS reopens itself) is returned unchanged.
fn follow_magic_final(kp: KPath) -> Result<KPath, isize> {
    let b = kp.bytes();
    if let Some((pid, rest)) = proc_magic(b, b"exe") {
        if rest.is_empty() {
            if sched::exists_probe(pid) != 0 { return Err(-2); }
            if !proc_link_allowed(pid) { return Err(-13); }
            let mut kb: Vec<u8> = alloc::vec![0u8; HOST_PATH_MAX];
            return Ok(match sched::exe_path(sched::tgid_of(pid), &mut kb) {
                Some(n) => KPath::raw(&kb[..n]),
                None => KPath::raw(b"/bin/init"),
            });
        }
    }
    if let Some((pid, rest)) = proc_magic(b, b"cwd") {
        if rest.is_empty() {
            if sched::exists_probe(pid) == 0 && !proc_link_allowed(pid) { return Err(-13); }
            let c = sched::cwd_arc_of(pid).ok_or(-2isize)?;
            return KPath::from_ns(c.to_vec());
        }
    }
    if let Some((pid, rest)) = proc_magic(b, b"root") {
        if rest.is_empty() {
            if sched::exists_probe(pid) != 0 { return Err(-2); }
            if !proc_link_allowed(pid) { return Err(-13); }
            let r = sched::root_arc_of(pid).map(|r| r.to_vec()).unwrap_or_else(|| b"/".to_vec());
            return KPath::from_ns(r);
        }
    }
    // /proc/<own pid>/fd/N naming a file on a mounted filesystem: reopen it
    // by its path. The VFS reopens tmpfs files and memfds itself (by
    // identity, which a path cannot do for an unlinked memfd), so those keep
    // the /proc path; another process's table is not reachable from here.
    if let Some((fd, below)) = proc_own_fd(b) {
        if below.is_empty() {
            let mounted = fd < net_server::SOCK_FD_BASE && matches!(
                vfs::vfs_get_node_kind(current_pid(), fd),
                Some(vfs::VnodeKind::MountedFile { .. }));
            if mounted {
                if let Some(host) = fd_host_path(fd) { return Ok(KPath::raw(&host)); }
            } else if !b.starts_with(b"/proc/self/fd/") {
                // The VFS reopens by identity only under /proc/self/fd: give
                // it /proc/<own pid>/fd/N (the Claude Code CLI reads its task
                // output that way) in that spelling.
                return Ok(KPath::raw(alloc::format!("/proc/self/fd/{}", fd).as_bytes()));
            }
        }
    }
    Ok(kp)
}

const RESOLVE_NO_XDEV:       u64 = 0x01;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS:   u64 = 0x04;
const RESOLVE_BENEATH:       u64 = 0x08;
const RESOLVE_IN_ROOT:       u64 = 0x10;
const RESOLVE_CACHED:        u64 = 0x20;

/// openat2(dirfd, path, how, size) — `struct open_how { u64 flags; u64 mode;
/// u64 resolve; }`, size-versioned like every extensible-struct syscall.
///
/// Unlike openat, unknown open flags, a mode without O_CREAT/O_TMPFILE, and
/// O_PATH with anything but O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC are EINVAL.
/// The RESOLVE_* flags are implemented by a kernel-side walk
/// ([`resolve_scoped`]) that looks at every component, since the normal path
/// resolution is lexical and leaves symlinks to the filesystems.
/// RESOLVE_CACHED is accepted and simply resolves (every lookup here is
/// "cached" in the sense that it cannot block on anything but the
/// filesystem server it would block on anyway).
fn sys_openat2(dirfd: usize, path_ptr: usize, how_ptr: usize, size: usize) -> isize {
    const VER0: usize = 24;
    if size < VER0 { return -22; }
    if size > 4096 { return -7; } // E2BIG
    if !validate_user_buf(how_ptr, size) { return -14; }
    let mut raw = [0u8; VER0];
    if unsafe { sched::uaccess::copy_from_user(raw.as_mut_ptr(), how_ptr, VER0) } != 0 { return -14; }
    if size > VER0 {
        let mut rest = [0u8; 256];
        let mut off = VER0;
        while off < size {
            let n = (size - off).min(rest.len());
            if unsafe { sched::uaccess::copy_from_user(rest.as_mut_ptr(), how_ptr + off, n) } != 0 {
                return -14;
            }
            if rest[..n].iter().any(|&b| b != 0) { return -7; } // E2BIG
            off += n;
        }
    }
    let f = |i: usize| u64::from_ne_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
    let (uflags, mode, resolve) = (f(0), f(1), f(2));

    // build_open_flags(), openat2 flavour. The flag check runs on the
    // x86_64 layout the VFS uses (the remap is a bijection on these bits).
    if uflags >> 32 != 0 { return -22; }
    let flags = open_flags_from_user(uflags as usize);
    const O_CREAT: usize = 0o100;
    const O_TRUNC: usize = 0o1000;
    const O_DIRECTORY: usize = 0o200000;
    const O_NOFOLLOW: usize = 0o400000;
    const O_CLOEXEC: usize = 0o2000000;
    const O_PATH: usize = 0o10000000;
    const O_TMPFILE: usize = 0o20000000 | O_DIRECTORY;
    const VALID: usize = 0o3 | O_CREAT | 0o200 | 0o400 | O_TRUNC | 0o2000 | 0o4000
        | 0o4010000 /* O_SYNC */ | 0o10000 | 0o20000 | 0o40000 | 0o100000 | O_DIRECTORY
        | O_NOFOLLOW | 0o1000000 | O_CLOEXEC | O_PATH | O_TMPFILE;
    if flags & !VALID != 0 { return -22; }
    if flags & O_CREAT != 0 || flags & O_TMPFILE == O_TMPFILE {
        if mode & !0o7777 != 0 { return -22; }
    } else if mode != 0 {
        return -22;
    }
    if flags & O_PATH != 0 && flags & !(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC | O_PATH) != 0 {
        return -22;
    }
    const VALID_RESOLVE: u64 = RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS
        | RESOLVE_BENEATH | RESOLVE_IN_ROOT | RESOLVE_CACHED;
    if resolve & !VALID_RESOLVE != 0 { return -22; }
    if resolve & RESOLVE_BENEATH != 0 && resolve & RESOLVE_IN_ROOT != 0 { return -22; }
    if resolve & RESOLVE_CACHED != 0 && flags & (O_CREAT | O_TRUNC | O_TMPFILE) != 0 { return -22; }

    let resolve = resolve & !RESOLVE_CACHED;
    if resolve == 0 {
        let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
        let path = if flags & O_NOFOLLOW == 0 {
            match follow_magic_final(path) { Ok(p) => p, Err(e) => return e }
        } else { path };
        return vfs_open_resolved(path.ptr(), flags, mode as usize);
    }
    let raw = match copy_user_path(path_ptr) { Ok(v) => v, Err(e) => return e };
    if raw.is_empty() { return -2; }
    // O_NOFOLLOW (and O_PATH|O_NOFOLLOW) acts on a final symlink itself.
    let follow_final = flags & O_NOFOLLOW == 0;
    let ns = match resolve_scoped(dirfd, &raw, resolve, follow_final) { Ok(p) => p, Err(e) => return e };
    let kp = match KPath::from_ns(ns) { Ok(k) => k, Err(e) => return e };
    let kp = if follow_final {
        match follow_magic_final(kp) { Ok(k) => k, Err(e) => return e }
    } else { kp };
    // Every symlink on the way was resolved above; a final one that appeared
    // since must not be followed behind the walk's back.
    let flags = if resolve & RESOLVE_NO_SYMLINKS != 0 && flags & O_PATH == 0 { flags | O_NOFOLLOW } else { flags };
    vfs_open_resolved(kp.ptr(), flags, mode as usize)
}

/// lstat a namespace path: `Ok(st_mode, st_dev)` or the errno. Synthetic
/// trees that only answer stat (parts of /proc) are stat'ed instead: they
/// hold no symlinks a walk could follow.
fn ns_lstat(ns: &[u8]) -> Result<(u32, u64), isize> {
    let kp = KPath::from_ns(ns.to_vec())?;
    let mut st = [0u8; STAT_SIZE];
    let m = make_vfs_msg(vfs::VFS_LSTAT, &[kp.ptr() as u64, st.as_mut_ptr() as u64]);
    let mut r = vfs_reply_val(&vfs::handle(&m, current_pid()));
    let in_proc = kp.bytes() == b"/proc" || kp.bytes().starts_with(b"/proc/");
    if r < 0 && (r != -2 || in_proc) {
        let m = make_vfs_msg(vfs::VFS_STAT, &[kp.ptr() as u64, st.as_mut_ptr() as u64]);
        r = vfs_reply_val(&vfs::handle(&m, current_pid()));
    }
    // The /proc/<pid> directories are generated on demand and not every one
    // answers stat; they are directories all the same.
    if r < 0 && in_proc {
        return Ok((0o040555, 0));
    }
    if r < 0 { return Err(r); }
    let dev = u64::from_ne_bytes(st[0..8].try_into().unwrap());
    Ok((vfs::read_stat_mode(st.as_ptr() as usize), dev))
}

/// The body of symlink `ns` (namespace path).
fn ns_readlink(ns: &[u8]) -> Result<Vec<u8>, isize> {
    let kp = KPath::from_ns(ns.to_vec())?;
    let mut buf: Vec<u8> = alloc::vec![0u8; KPATH_MAX];
    let m = make_vfs_msg(vfs::VFS_READLINK, &[kp.ptr() as u64, buf.as_mut_ptr() as u64, KPATH_MAX as u64]);
    let r = vfs_reply_val(&vfs::handle(&m, current_pid()));
    if r < 0 { return Err(r); }
    if r == 0 { return Err(-2); }
    buf.truncate(r as usize);
    Ok(buf)
}

/// True for the /proc "magic links": `/proc/<pid>/{exe,cwd,root}` and
/// `/proc/<pid>/fd/<n>`.
fn is_magic_link(ns: &[u8]) -> bool {
    let b = backing_of(ns).unwrap_or_else(|| ns.to_vec());
    if proc_ns_link(&b).is_some() { return true; }
    for what in [&b"exe"[..], b"cwd", b"root"] {
        if let Some((_, rest)) = proc_magic(&b, what) { if rest.is_empty() { return true; } }
    }
    if let Some((_, rest)) = proc_magic(&b, b"fd") {
        let n = rest.strip_prefix(b"/").unwrap_or(b"");
        return !n.is_empty() && n.iter().all(|d| d.is_ascii_digit());
    }
    false
}

/// openat2's RESOLVE_* path walk, component by component, returning the
/// namespace path to open (every symlink on the way resolved).
///
/// * RESOLVE_BENEATH: the walk may not leave `dirfd`'s directory — an
///   absolute path, an absolute symlink, or `..` above it is EXDEV.
/// * RESOLVE_IN_ROOT: `dirfd` acts as the root — absolute paths and symlinks
///   start there, and `..` stops there.
/// * RESOLVE_NO_SYMLINKS: any symlink on the way is ELOOP (a final one only
///   when it would be followed).
/// * RESOLVE_NO_MAGICLINKS: the /proc magic links are ELOOP (EXDEV under a
///   scope, as Linux).
/// * RESOLVE_NO_XDEV: every component must be on `dirfd`'s filesystem
///   (same st_dev), else EXDEV.
fn resolve_scoped(dirfd: usize, raw: &[u8], resolve: u64, follow_final: bool) -> Result<Vec<u8>, isize> {
    const S_IFMT: u32 = 0o170000;
    const S_IFDIR: u32 = 0o040000;
    const S_IFLNK: u32 = 0o120000;
    const MAXSYMLINKS: u32 = 40;
    let scoped = resolve & (RESOLVE_BENEATH | RESOLVE_IN_ROOT) != 0;
    let dirfd = dirfd as u32 as i32 as isize as usize; // an `int`, as resolve_at_ns

    // The starting directory (namespace path).
    let base: Vec<u8> = if dirfd == AT_FDCWD {
        resolve_ns(b".")?
    } else {
        match fd_abs_path(dirfd) {
            Some(v) => resolve_ns(&v)?,
            None => return Err(-9), // EBADF (or not a directory we can name)
        }
    };
    let jail_root: Vec<u8> = resolve_ns(b"/")?;
    // The point `..` and absolute names are anchored at.
    let anchor: Vec<u8> = if scoped { base.clone() } else { jail_root.clone() };

    let base_dev = if resolve & RESOLVE_NO_XDEV != 0 { Some(ns_lstat(&base)?.1) } else { None };

    // Remaining components, last one at the bottom of the stack.
    let mut todo: Vec<Vec<u8>> = Vec::new();
    let push_path = |todo: &mut Vec<Vec<u8>>, p: &[u8]| {
        let comps: Vec<&[u8]> = p.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
        for c in comps.into_iter().rev() { todo.push(c.to_vec()); }
    };
    let mut cur: Vec<u8>;
    if raw.first() == Some(&b'/') {
        if resolve & RESOLVE_BENEATH != 0 { return Err(-18); } // EXDEV
        cur = anchor.clone();
    } else {
        cur = base.clone();
    }
    push_path(&mut todo, raw);
    let mut hops = 0u32;

    while let Some(comp) = todo.pop() {
        if comp == b"." { continue; }
        if comp == b".." {
            if cur == anchor || (cur.len() <= anchor.len() && anchor.starts_with(&cur)) {
                if resolve & RESOLVE_BENEATH != 0 { return Err(-18); }
                cur = anchor.clone();
                continue;
            }
            let mut last = cur.len() - 1;
            while last > 0 && cur[last] != b'/' { last -= 1; }
            cur.truncate(if last == 0 { 1 } else { last });
            if let Some(d) = base_dev {
                if ns_lstat(&cur)?.1 != d { return Err(-18); }
            }
            continue;
        }
        if comp.len() > NAME_MAX { return Err(-36); }
        let mut next = cur.clone();
        if next.last() != Some(&b'/') { next.push(b'/'); }
        next.extend_from_slice(&comp);
        if next.len() >= HOST_PATH_MAX { return Err(-36); }
        let is_last = todo.is_empty();

        if is_magic_link(&next) && (is_last && !follow_final) == false {
            if resolve & RESOLVE_NO_MAGICLINKS != 0 { return Err(-40); }
            if scoped { return Err(-18); }
        }
        let (mode, dev) = match ns_lstat(&next) {
            Ok(x) => x,
            // A missing final component is for O_CREAT (or the open) to judge.
            Err(-2) if is_last => { cur = next; break; }
            Err(e) => return Err(e),
        };
        if let Some(d) = base_dev { if dev != d { return Err(-18); } }
        if mode & S_IFMT == S_IFLNK && !(is_last && !follow_final) {
            if resolve & RESOLVE_NO_SYMLINKS != 0 { return Err(-40); } // ELOOP
            hops += 1;
            if hops > MAXSYMLINKS { return Err(-40); }
            let target = ns_readlink(&next)?;
            if target.first() == Some(&b'/') {
                if resolve & RESOLVE_BENEATH != 0 { return Err(-18); }
                cur = anchor.clone();
            }
            push_path(&mut todo, &target);
            continue;
        }
        if !is_last && mode & S_IFMT != S_IFDIR && mode & S_IFMT != S_IFLNK {
            return Err(-20); // ENOTDIR
        }
        cur = next;
    }
    Ok(cur)
}

/// Close the hidden socket behind a VFS-range alias (the alias entry itself is
/// the caller's to remove). epoll_ctl on an alias registers the socket it
/// names (see `sock_alias_args`), so those registrations are keyed by the
/// hidden number: drop them first, before the number can be reused.
fn close_sock_alias_target(pid: u32, sock: usize) {
    if !fd_private() { epoll_release_fd(pid, sched::current_tgid(), sock); }
    let msg = make_vfs_msg(net_server::NET_CLOSE, &[sock as u64]);
    let _ = net_server::handle(&msg, pid);
}

fn sys_close(fd: usize) -> isize {
    let pid = current_pid();
    let h = fd_handle(fd);
    if h == vfs::BAD_FD { return -9; } // a raw handle number is no descriptor
    // An unredirected fd 0/1/2 is the serial console, which has no fd-table
    // entry (see fd_is_console_stdio), so the VFS would answer EBADF. Linux
    // closes the console descriptor successfully, and programs check: git
    // ends every command with close(1) and died with "fatal: close failed on
    // standard output: Bad file descriptor" at the serial login. From then on
    // the number is closed (`vfs::close_console`): a write, fstat or second
    // close is EBADF, and the next open may reuse it, as on Linux.
    if fd <= 2 && !vfs::fd_redirected(pid, fd) {
        if !fd_private() { epoll_release_fd(pid, sched::current_tgid(), fd); }
        vfs::close_console(pid, fd);
        return 0;
    }
    if is_sock_handle(h) || is_epoll_handle(h) {
        // A socket or epoll fd: the handle behind it first (its epoll
        // registrations, keyed by the handle, go with it), then the entry.
        close_handle(pid, h);
    } else {
        // The fd's epoll registrations go BEFORE the close: once the VFS frees
        // the number, a sibling thread can be handed it for a new file and
        // register that — a forget after the close would then delete the
        // newcomer's registration (measured: a notify/mio waker lost that way
        // hung cosmic-term's exit in tokio's blocking-pool shutdown).
        if !fd_private() { epoll_release_fd(pid, sched::current_tgid(), fd); }
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
    // A user destination gets a kernel copy, filled by the kernel-buffer
    // path and copied out once (PAN-safe).
    if user_dest {
        if !validate_user_buf(statbuf_ptr, STAT_SIZE) { return -14; }
        let mut kb = [0u64; STAT_SIZE.div_ceil(8)];
        let r = fstat_into(fd, kb.as_mut_ptr() as usize, false);
        if r >= 0 && unsafe { ua::copy_to_user(statbuf_ptr, kb.as_ptr() as *const u8, STAT_SIZE) } != 0 { return -14; }
        return r;
    }
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

    // A socket or an epoll fd (`fd_handle_args` already translated the
    // descriptor to its handle): neither has a VFS object to ask.
    if is_sock_handle(fd) || is_epoll_handle(fd) {
        return match handle_stat(pid, fd) {
            Some((mode, ino, dev)) => {
                vfs::write_stat_full(statbuf_ptr, mode, 1, 0, ino,
                    sched::current_euid(), sched::current_egid());
                // st_dev is the first field of both arches' struct stat.
                unsafe { (statbuf_ptr as *mut u64).write_unaligned(dev); }
                0
            }
            None => -9, // EBADF
        };
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
    flags: usize,
    _data_ptr: usize,
) -> isize {
    // mount(2) MS_REMOUNT: change the flags of an existing mount; source and
    // fstype are ignored (and may be NULL, as `mount -o remount,ro /` passes
    // them), so this is decided before either is read. Only MS_RDONLY is
    // honoured: f2fs commits a clean-unmount checkpoint on the way to
    // read-only (init's last step before reboot(2)).
    const MS_RDONLY:  usize = 1;
    const MS_NOSUID:  usize = 2;
    const MS_REMOUNT: usize = 32;
    // Every form of mount(2) needs CAP_SYS_ADMIN (Linux `may_mount`).
    if sched::current_euid() != 0 { return -1; } // EPERM
    if flags & MS_REMOUNT != 0 {
        let target_path = match resolve_user_path(target_ptr) { Ok(p) => p, Err(e) => return e };
        let t = match core::str::from_utf8(target_path.bytes()) { Ok(s) => s, Err(_) => return -22 };
        // A tmpfs root (/tmp, /dev/shm, /run/user): nosuid/nodev/noexec.
        if vfs::set_tmpfs_root_flags(target_path.bytes(), flags) { return 0; }
        let t = if t.len() > 1 { t.trim_end_matches('/') } else { t };
        let mounts = vfs::list_mounts();
        let entry = match mounts.iter().find(|e| e.in_use && e.prefix == t) {
            Some(e) => *e,
            // An alias mount (proc/devtmpfs/sysfs/bind) has no flags to
            // change; anything else is not a mount point.
            None if vfs::alias::is_mountpoint(target_path.ns_bytes()) => return 0,
            None => return -22, // EINVAL: not a mount point
        };
        // A remount's flags replace the old ones, nosuid included.
        vfs::set_mount_nosuid(entry.prefix, flags & MS_NOSUID != 0);
        if entry.fstype != "f2fs" { return 0; } // tmpfs/proc: nothing to commit
        return if f2fs_server::remount_by_port(entry.port, flags & MS_RDONLY != 0) { 0 } else { -22 };
    }
    const MS_BIND: usize = 4096;
    // The mount point is a real path — resolve it against the cwd (and the
    // caller's chroot) so `mount /dev/vda mnt` and `mount /dev/vda /mnt` agree.
    let target_path = match resolve_user_path(target_ptr) { Ok(p) => p, Err(e) => return e };

    // Alias mounts: a bind mount, or one of the filesystems this kernel serves
    // under a fixed prefix (proc, devtmpfs, sysfs, devpts). Mounting one
    // somewhere else — typically into a chroot — makes that place show the
    // same tree (see vfs::alias). fstype is ignored for MS_BIND, as on Linux.
    let fstype_raw = if flags & MS_BIND != 0 || fstype_ptr == 0 {
        Vec::new()
    } else {
        match read_user_short_str(fstype_ptr) { Ok(v) => v, Err(e) => return e }
    };
    let alias_src: Option<(Vec<u8>, &'static str)> = if flags & MS_BIND != 0 {
        let src = match resolve_user_path(source_ptr) { Ok(p) => p, Err(e) => return e };
        Some((src.bytes().to_vec(), "none"))
    } else {
        match &fstype_raw[..] {
            b"proc"     => Some((b"/proc".to_vec(), "proc")),
            b"devtmpfs" => Some((b"/dev".to_vec(), "devtmpfs")),
            b"sysfs"    => Some((b"/sys".to_vec(), "sysfs")),
            b"devpts"   => Some((b"/dev/pts".to_vec(), "devpts")),
            _ => None,
        }
    };
    if let Some((src, fstype)) = alias_src {
        let mp = target_path.ns_bytes();
        if mp == b"/" { return -16; } // EBUSY: no aliasing the root itself
        // The mount point must exist and, for these trees, be a directory
        // (a bind of a file onto a file is allowed too).
        const S_IFMT: u32 = 0o170000;
        const S_IFDIR: u32 = 0o040000;
        let mut st = [0u8; STAT_SIZE];
        let m = make_vfs_msg(vfs::VFS_STAT, &[target_path.ptr() as u64, st.as_mut_ptr() as u64]);
        let r = vfs_reply_val(&vfs::handle(&m, current_pid()));
        if r < 0 { return r; }
        let tdir = vfs::read_stat_mode(st.as_ptr() as usize) & S_IFMT == S_IFDIR;
        if flags & MS_BIND != 0 {
            let sk = KPath::raw(&src);
            let mut ss = [0u8; STAT_SIZE];
            let m = make_vfs_msg(vfs::VFS_STAT, &[sk.ptr() as u64, ss.as_mut_ptr() as u64]);
            let r = vfs_reply_val(&vfs::handle(&m, current_pid()));
            if r < 0 { return r; }
            let sdir = vfs::read_stat_mode(ss.as_ptr() as usize) & S_IFMT == S_IFDIR;
            if sdir != tdir { return if sdir { -20 } else { -21 }; } // ENOTDIR / EISDIR
        } else if !tdir {
            return -20; // ENOTDIR
        }
        let device: Vec<u8> = if flags & MS_BIND != 0 {
            src.clone()
        } else if source_ptr != 0 {
            read_user_short_str(source_ptr).unwrap_or_else(|_| fstype.as_bytes().to_vec())
        } else {
            fstype.as_bytes().to_vec()
        };
        return match vfs::alias::add(mp, &src, fstype, &device) { Ok(()) => 0, Err(e) => e as isize };
    }

    let source_raw = match read_user_short_str(source_ptr) { Ok(v) => v, Err(e) => return e };
    let source_str = match core::str::from_utf8(&source_raw) {
        Ok(s) => s,
        Err(_) => return -22, // EINVAL
    };
    let target_str = match core::str::from_utf8(target_path.bytes()) {
        Ok(s) => s,
        Err(_) => return -22, // EINVAL
    };
    let fstype_str = match core::str::from_utf8(&fstype_raw) {
        Ok(s) => s,
        Err(_) => return -22, // EINVAL
    };

    // The tmpfs roots are built in: mounting tmpfs onto one sets its
    // nosuid/nodev/noexec flags (its contents stay). Elsewhere: EINVAL.
    if fstype_str == "tmpfs" {
        return if vfs::set_tmpfs_root_flags(target_path.bytes(), flags) { 0 } else { -22 };
    }
    if fstype_str != "f2fs" {
        return -22; // EINVAL
    }

    // Source forms: `/dev/root` (resolved now — see blkdev::resolve_root),
    // `/dev/<registry name>` (vdb, sda2, …), `LABEL=`/`PARTUUID=`, or a bare
    // registry index.
    let dev_idx = if source_str == "/dev/root" {
        match drivers::blkdev::resolve_root(crate::boot_cmdline(), crate::boot_disk()) {
            Ok(i) => {
                crate::serial_print_str("[BLK] /dev/root -> ");
                crate::serial_print_str(&drivers::blkdev::dev_path(i)[5..]);
                crate::serial_print_str("\n");
                i
            }
            // ENODEV: nothing yet — a USB disk may still be enumerating, and
            // init retries on exactly this errno.
            Err(drivers::blkdev::RootErr::NotFound) => return -19,
            Err(drivers::blkdev::RootErr::Ambiguous) => return -76, // ENOTUNIQ
        }
    } else if let Some(r) = drivers::blkdev::resolve_source(source_str, crate::boot_cmdline(), crate::boot_disk()) {
        // `LABEL=<f2fs label>` / `PARTUUID=<id>`: fstab's /home entry is
        // `LABEL=leandros-home`, on QEMU's vdc or a stick's home partition.
        match r {
            Ok(i) => {
                crate::serial_print_str("[BLK] ");
                crate::serial_print_str(source_str);
                crate::serial_print_str(" -> ");
                crate::serial_print_str(&drivers::blkdev::dev_path(i)[5..]);
                crate::serial_print_str("\n");
                i
            }
            Err(drivers::blkdev::RootErr::NotFound) => return -2, // ENOENT
            Err(drivers::blkdev::RootErr::Ambiguous) => return -76, // ENOTUNIQ
        }
    } else if let Some(name) = source_str.strip_prefix("/dev/") {
        match drivers::blkdev::lookup_name(name) {
            Some(i) => i,
            None => return -6, // ENXIO, as an out-of-range /dev/vdX always was
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

    // A disk the block layer refuses to write (NVMe without nvme.rw=1)
    // mounts only read-only, as Linux answers EROFS for a read-write mount
    // of a read-only block device.
    let read_only = flags & MS_RDONLY != 0;
    if !read_only && drivers::blkdev::read_only(dev_idx) {
        crate::serial_print_str("[BLK] mount: ");
        crate::serial_print_str(&drivers::blkdev::dev_path(dev_idx)[5..]);
        crate::serial_print_str(" is read-only; refusing a read-write mount (use -o ro)\n");
        return -30; // EROFS
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

    // The mount's port belongs to the kernel (pid 0), not to the caller: a
    // port dies with its owner (`ipc::port::release_by_owner`), and the
    // caller is usually a short-lived `mount` process. Owned by it, the
    // volume stayed in the mount table after `mount` exited while its port
    // was gone, so every later stat/open/getdents64 under it failed in
    // `call_port` (it was reported as ENOMEM). Only init, which never
    // exits, had working mounts.
    if let Some(_port) = f2fs_server::mount_with(dev_idx, mount_point, 0, read_only) {
        vfs::set_mount_nosuid(mount_point, flags & MS_NOSUID != 0);
        0
    } else {
        -1
    }
}

fn sys_umount2(target_ptr: usize, _flags: usize) -> isize {
    if sched::current_euid() != 0 { return -1; } // EPERM (CAP_SYS_ADMIN)
    let target_path = match resolve_user_path(target_ptr) { Ok(p) => p, Err(e) => return e };
    // An alias mount (a chroot's /proc, a bind mount) is named by where the
    // caller sees it, i.e. its namespace path.
    if vfs::alias::remove(target_path.ns_bytes()) { return 0; }
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
    if sched::current_euid() != 0 { return -1; } // EPERM (CAP_SYS_ADMIN)
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
        let _ = sched::uaccess::write_user::<u64>(out_ptr, info.total_blocks);
        let _ = sched::uaccess::write_user::<u32>((out_ptr + 8), info.block_size);
        let fstype = info.fstype.unwrap_or("");
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 12), if fstype.is_empty() { 0 } else { 1 });
        let mut name_buf = [0u8; 8];
        let n = fstype.len().min(8);
        name_buf[..n].copy_from_slice(&fstype.as_bytes()[..n]);
        let _ = sched::uaccess::write_user::<[u8; 8]>(out_ptr + 13, name_buf);
    }
    0
}

fn sys_blkdev_name(index: usize, out_ptr: usize) -> isize {
    let d = match drivers::blkdev::describe(index) {
        Some(d) if !d.gone => d,
        _ => return -19, // ENODEV (never registered, or unplugged)
    };
    let name = d.name();
    if !validate_user_buf(out_ptr, drivers::blkdev::NAME_MAX) { return -14; } // EFAULT
    if unsafe { sched::uaccess::copy_to_user(out_ptr, name.as_ptr(), name.len()) } != 0 { return -14; }
    name.len() as isize
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
        let _ = sched::uaccess::write_user::<u8>(out_ptr, dev.bus);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 1), dev.dev);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 2), dev.func);
        let _ = sched::uaccess::write_user::<u16>((out_ptr + 4), dev.vendor_id);
        let _ = sched::uaccess::write_user::<u16>((out_ptr + 6), dev.device_id);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 8), dev.class);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 9), dev.subclass);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 10), dev.prog_if);
    }
    0
}

/// out layout: bus:u8@0, address:u8@1, vendor_id:u16@4, product_id:u16@6, class:u8@8,
/// role:u8@9 (usb_hcd::ROLE_* bits), port:u8@10, speed:u8@11 (12 bytes)
const USBDEV_INFO_SIZE: usize = 12;

fn sys_usbdev_info(index: usize, out_ptr: usize) -> isize {
    if !validate_user_buf(out_ptr, USBDEV_INFO_SIZE) { return -14; } // EFAULT
    let info = match drivers::usb_hcd::device_info(index) {
        Some(i) => i,
        None => return -19, // ENODEV
    };
    unsafe {
        let _ = sched::uaccess::write_user::<u8>(out_ptr, info.bus);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 1), info.address);
        let _ = sched::uaccess::write_user::<u16>((out_ptr + 4), info.vendor_id);
        let _ = sched::uaccess::write_user::<u16>((out_ptr + 6), info.product_id);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 8), info.class);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 9), info.role);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 10), info.port);
        let _ = sched::uaccess::write_user::<u8>((out_ptr + 11), info.speed);
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
        if !sched::uaccess::write_user::<[u8; 32]>(out_ptr, mp) { return -14; }

        let mut dev = [0u8; 16];
        let n = entry.device.len().min(16);
        dev[..n].copy_from_slice(&entry.device.as_bytes()[..n]);
        if !sched::uaccess::write_user::<[u8; 16]>(out_ptr + 32, dev) { return -14; }

        let mut fst = [0u8; 8];
        let n = entry.fstype.len().min(8);
        fst[..n].copy_from_slice(&entry.fstype.as_bytes()[..n]);
        if !sched::uaccess::write_user::<[u8; 8]>(out_ptr + 48, fst) { return -14; }
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
    // A user destination gets a kernel copy, filled by the kernel-buffer
    // path and copied out once (PAN-safe).
    if user_dest {
        if !validate_user_buf(statbuf_ptr, STAT_SIZE) { return -14; }
        let mut kb = [0u64; STAT_SIZE.div_ceil(8)];
        let r = fstatat_into(dirfd, path_ptr, kb.as_mut_ptr() as usize, flags, false);
        if r >= 0 && unsafe { ua::copy_to_user(statbuf_ptr, kb.as_ptr() as *const u8, STAT_SIZE) } != 0 { return -14; }
        return r;
    }
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
            && ur::<u8>(path_ptr) == 0;
        if path_ptr == 0 || empty { return fstat_into(dirfd, statbuf_ptr, user_dest); }
    }

    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let path = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match follow_magic_final(path) { Ok(p) => p, Err(e) => return e }
    } else {
        // lstat of a /proc magic link describes the link itself, as on Linux
        // (`lstat /proc/self/exe` was ENOENT: nothing below answered for it).
        if let Some(r) = lstat_magic_link(path.ns_bytes(), statbuf_ptr) { return r; }
        path
    };
    let trace = SC_STATS && sc_stat_path_focus();
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
    let stat_tag = if flags & AT_SYMLINK_NOFOLLOW != 0 { vfs::VFS_LSTAT } else { vfs::VFS_STAT };
    let smsg = make_vfs_msg(stat_tag, &[path_ptr as u64, statbuf_ptr as u64]);
    let t0 = if trace { monotonic_ns() } else { 0 };
    let sr = vfs_reply_val(&vfs::handle(&smsg, pid));
    let t1 = if trace { monotonic_ns() } else { 0 };
    if sr >= 0 {
        if trace { sc_stat_path_trace(path.bytes(), sr, t1 - t0, 0, 0); }
        return 0;
    }

    // Fallbacks below only run when VFS_STAT could not describe the path.
    if vfs::is_directory(path_ptr) {
        // st_mode: S_IFDIR | 0755
        vfs::write_stat_full(statbuf_ptr, 0o040755, 2, 0, 0, 0, 0);
        return 0;
    }
    let t2 = if trace { monotonic_ns() } else { 0 };
    // Open path, use sys_fstat, then close.
    let omsg = make_vfs_msg(vfs::VFS_OPEN, &[path_ptr as u64, 0u64, 0]);
    let fd = vfs_reply_val(&vfs::handle(&omsg, pid));
    if trace { sc_stat_path_trace(path.bytes(), sr, t1 - t0, t2 - t1, monotonic_ns() - t2); }
    if fd < 0 { return fd; }
    let r = fstat_into(fd as usize, statbuf_ptr, user_dest);
    let cmsg = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]);
    let _ = vfs::handle(&cmsg, pid);
    r
}

pub(crate) fn sys_lseek_pub(fd: usize, offset: usize, whence: usize) -> isize { sys_lseek(fd, offset, whence) }

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
// Keyed by tgid, holding only the processes whose console flags are nonzero:
// a map on the heap, not a static array sized by MAX_PROCESSES (24 KiB of
// .bss until lane fdgaps, scanned up to its high-water mark on every read).
// The flags belong to the console's open file description on Linux, which a
// fork shares and an exec keeps: the child gets a copy at fork
// (`stdio_flags_fork`), exec leaves them alone, a leader's exit drops them.
const O_NONBLOCK: u32 = 0x800;

static STDIO_FLAGS: spin::Mutex<alloc::collections::BTreeMap<u32, u32>> =
    spin::Mutex::new(alloc::collections::BTreeMap::new());
/// Entries in STDIO_FLAGS (written under the lock): every console read asks
/// `stdio_nonblocking`, and almost no process ever sets a flag, so the common
/// case is one relaxed load and no lock.
static STDIO_FLAGS_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

#[inline]
fn stdio_flags_any() -> bool {
    STDIO_FLAGS_LEN.load(core::sync::atomic::Ordering::Relaxed) != 0
}

/// `tgid`'s console status flags (0 when none were set).
fn stdio_flags(tgid: u32) -> u32 {
    if !stdio_flags_any() { return 0; }
    STDIO_FLAGS.lock().get(&tgid).copied().unwrap_or(0)
}

fn stdio_nonblocking(pid: u32) -> bool {
    // Keyed by tgid — stdio flags are process-wide state, and a worker
    // thread must observe O_NONBLOCK set by the main thread (and vice versa).
    if !stdio_flags_any() { return false; }
    stdio_flags(sched::tgid_of(pid)) & O_NONBLOCK != 0
}

/// Drop `pid`'s console flags when it is a process (leader) going away; a
/// plain thread's exit leaves its group's flags alone (they are keyed by
/// tgid, which only a leader's pid matches).
fn stdio_flags_close_all(pid: u32) {
    if !stdio_flags_any() { return; }
    let mut tbl = STDIO_FLAGS.lock();
    tbl.remove(&pid);
    STDIO_FLAGS_LEN.store(tbl.len(), core::sync::atomic::Ordering::Relaxed);
}

fn set_stdio_flags(pid: u32, flags: u32) {
    set_stdio_flags_tgid(sched::tgid_of(pid), flags); // process-wide — see stdio_nonblocking
}

/// fork: the child shares the parent's console description on Linux, so it
/// starts with the parent's flags.
fn stdio_flags_fork(parent_tgid: u32, child_tgid: u32) {
    let f = stdio_flags(parent_tgid);
    if f != 0 { set_stdio_flags_tgid(child_tgid, f); }
}

fn set_stdio_flags_tgid(tgid: u32, flags: u32) {
    let mut tbl = STDIO_FLAGS.lock();
    if flags == 0 { tbl.remove(&tgid); } else { tbl.insert(tgid, flags); }
    STDIO_FLAGS_LEN.store(tbl.len(), core::sync::atomic::Ordering::Relaxed);
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
    if is_epoll_handle(fd) {
        const F_DUPFD: usize = 0;
        const F_DUPFD_CLOEXEC: usize = 1030;
        let r = epoll_fcntl(fd, cmd, arg);
        // F_DUPFD{,_CLOEXEC} made a new epoll handle: give it a descriptor.
        if (cmd == F_DUPFD || cmd == F_DUPFD_CLOEXEC) && r >= 0 {
            return publish_handle(pid, r as usize, arg);
        }
        return r;
    }
    if is_sock_handle(fd) {
        const F_DUPFD: usize = 0;
        const F_GETFD: usize = 1;
        const F_SETFD: usize = 2;
        const F_GETFL: usize = 3;
        const F_SETFL: usize = 4;
        const F_DUPFD_CLOEXEC: usize = 1030;
        return match cmd {
            F_DUPFD | F_DUPFD_CLOEXEC => {
                // Linux checks the minimum before duplicating anything.
                if arg >= vfs::fd_limit(pid) { return -22; } // EINVAL
                let msg = make_vfs_msg(net_server::NET_DUP,
                    &[fd as u64, (cmd == F_DUPFD_CLOEXEC) as u64]);
                let d = net_reply_val(&net_server::handle(&msg, pid));
                if d < 0 { return d; }
                publish_handle(pid, d as usize, arg)
            }
            // Status flags in the caller's layout, as for a VFS fd below.
            F_SETFL => {
                let msg = make_vfs_msg(net_server::NET_SETFL, &[fd as u64, open_flags_from_user(arg) as u64]);
                net_reply_val(&net_server::handle(&msg, pid))
            }
            F_GETFL => {
                let msg = make_vfs_msg(net_server::NET_GETFL, &[fd as u64]);
                let r = net_reply_val(&net_server::handle(&msg, pid));
                if r >= 0 { open_flags_to_user(r as usize) as isize } else { r }
            }
            // F_SETFD/F_GETFD must reach the net server's per-socket cloexec
            // flag: a program that clears FD_CLOEXEC on an inherited SOCK_CLOEXEC
            // socket before execve (launch_pad's with_fds → cosmic notification
            // sockets) relies on it, else the socket stays cloexec and the
            // execve sweep closes it, leaving the child with EBADF.
            F_SETFD => {
                let msg = make_vfs_msg(net_server::NET_SETFD, &[fd as u64, arg as u64]);
                net_reply_val(&net_server::handle(&msg, pid))
            }
            F_GETFD => {
                let msg = make_vfs_msg(net_server::NET_GETFD, &[fd as u64]);
                net_reply_val(&net_server::handle(&msg, pid))
            }
            _ => 0,
        };
    }
    // Only the implicit console has the per-process stdio flags; an fd 0-2
    // redirected onto a real object (or closed) is an ordinary descriptor.
    if fd <= 2 && !vfs::fd_redirected(pid, fd) {
        const F_GETFL: usize = 3;
        const F_SETFL: usize = 4;
        const F_DUPFD:         usize = 0;
        const F_DUPFD_CLOEXEC: usize = 1030;
        // Duplicating stdio must produce a real new fd (a /dev/std* proxy in
        // the VFS), not the 0 the catch-all below returns. std's
        // `BorrowedFd::try_clone_to_owned` is F_DUPFD_CLOEXEC, and answering it
        // with "0" hands the caller a second alias for stdin that it will later
        // close — which is how a redirect could wedge the shell's own console.
        if cmd == F_DUPFD || cmd == F_DUPFD_CLOEXEC {
            let msg = make_vfs_msg(vfs::VFS_FCNTL, &[fd as u64, cmd as u64, arg as u64]);
            return vfs_reply_val(&vfs::handle(&msg, pid));
        }
        return match cmd {
            F_GETFL => {
                // STDIO_FLAGS is per-process (TGID); F_SETFL's set_stdio_flags
                // canonicalizes but this sibling reader must match it, or a
                // non-leader thread's query reads flags=0 (looks blocking).
                stdio_flags(sched::tgid_of(pid)) as isize
            }
            F_SETFL => { set_stdio_flags(pid, arg as u32); 0 }
            _ => 0, // F_GETFD/F_SETFD/etc: no real close-on-exec semantics needed for stdio
        };
    }
    // F_GETFL/F_SETFL carry open(2) status flags: translate them between the
    // caller's layout and the VFS's, as open/openat do.
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    let varg = if cmd == F_SETFL { open_flags_from_user(arg) } else { arg };
    let msg = make_vfs_msg(vfs::VFS_FCNTL, &[fd as u64, cmd as u64, varg as u64]);
    let mut r = vfs_reply_val(&vfs::handle(&msg, pid));
    if cmd == F_GETFL && r >= 0 { r = open_flags_to_user(r as usize) as isize; }
    trace_fd("fcntl", fd, cmd, arg, r);
    r
}

fn sys_fchmod(fd: usize, mode: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_FCHMOD, &[fd as u64, mode as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// fchmodat2(dirfd, path, mode, flags): fchmodat with the flags argument the
/// original syscall never had. musl tries it first for any flagged fchmodat
/// and otherwise falls back to an O_PATH open plus chmod("/proc/self/fd/N"),
/// which needs /proc; GNU tar restores directory modes through exactly that
/// call (AT_SYMLINK_NOFOLLOW).
fn sys_fchmodat2(dirfd: usize, path_ptr: usize, mode: usize, flags: usize) -> isize {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 { return -22; } // EINVAL
    if flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
        let empty = path_ptr == 0
            || (validate_user_buf(path_ptr, 1) && unsafe { *(path_ptr as *const u8) } == 0);
        if empty { return sys_fchmod(dirfd, mode); }
    }
    sys_fchmodat(dirfd, path_ptr, mode, flags & AT_SYMLINK_NOFOLLOW)
}

fn sys_fchmodat(dirfd: usize, path_ptr: usize, mode: usize, flags: usize) -> isize {
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let tag = if flags & AT_SYMLINK_NOFOLLOW != 0 { vfs::VFS_LCHMOD } else { vfs::VFS_CHMOD };
    let msg = make_vfs_msg(tag, &[path.ptr() as u64, mode as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// Copy an xattr name (NUL-terminated, at most XATTR_NAME_MAX bytes) into
/// kernel memory, NUL included. The VFS and the mount servers read it with
/// their locks held, so they never see the user pointer. EFAULT for a bad
/// pointer, ERANGE for a name that is too long (Linux's errno for it).
fn copy_xattr_name(name: usize) -> Result<Vec<u8>, isize> {
    const XATTR_NAME_MAX: usize = 255;
    let mut v = match copy_user_str(name, XATTR_NAME_MAX + 1) {
        Ok(v) => v,
        Err(-36) => return Err(-34), // ERANGE
        Err(e) => return Err(e),
    };
    if v.try_reserve(1).is_err() { return Err(-12); }
    v.push(0);
    Ok(v)
}

/// After a getxattr/listxattr into a bounce buffer: copy the `r` bytes the
/// server produced out to `user` (a size-0 length query copies nothing).
fn copy_out_bounce(r: isize, size: usize, user: usize, kbuf: &[u8]) -> isize {
    if r <= 0 || size == 0 { return r; }
    let n = (r as usize).min(size);
    if unsafe { sched::uaccess::copy_to_user(user, kbuf.as_ptr(), n) } != 0 { return -14; }
    r
}

/// setxattr(path, name, value, size, flags) / lsetxattr / fsetxattr.
///
/// `target` is a path resolved via `resolve_user_path` for the non-fd forms,
/// or (for fsetxattr) an already-open fd forwarded to the VFS verbatim, same
/// as every other f-form syscall in this file. See VFS_SETXATTR /
/// VFS_LSETXATTR / VFS_FSETXATTR in servers/vfs/src/lib.rs for the wire
/// layout; size caps and errno values come from the shared `xattr` crate.
fn sys_setxattr(
    target: usize,
    name: usize,
    val: usize,
    size: usize,
    flags: usize,
    is_fd: bool,
    nofollow: bool,
) -> isize {
    if size > xattr::XATTR_SIZE_MAX { return -7; } // E2BIG

    let kname = match copy_xattr_name(name) { Ok(n) => n, Err(e) => return e };

    let mut kval: Vec<u8> = Vec::new();
    if size > 0 {
        if kval.try_reserve_exact(size).is_err() { return -12; }
        kval.resize(size, 0);
        if unsafe { sched::uaccess::copy_from_user(kval.as_mut_ptr(), val, size) } != 0 { return -14; }
    }

    let pid = current_pid();
    let kp;
    let arg0 = if is_fd {
        target as u64
    } else {
        kp = match resolve_user_path(target) { Ok(p) => p, Err(e) => return e };
        kp.ptr() as u64
    };
    let tag = if is_fd {
        vfs::VFS_FSETXATTR
    } else if nofollow {
        vfs::VFS_LSETXATTR
    } else {
        vfs::VFS_SETXATTR
    };
    let msg = make_vfs_msg(tag, &[arg0, kname.as_ptr() as u64, kval.as_ptr() as u64, size as u64, flags as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// getxattr(path, name, value, size) / lgetxattr / fgetxattr → value length or
/// -errno. `size == 0` (with a null or garbage `val`) is a length query, so
/// the value buffer is only validated/prefaulted when `size > 0`.
fn sys_getxattr(
    target: usize,
    name: usize,
    val: usize,
    size: usize,
    is_fd: bool,
    nofollow: bool,
) -> isize {
    let kname = match copy_xattr_name(name) { Ok(n) => n, Err(e) => return e };

    // Filled with the xattr/mount locks held: bounced, copied out below.
    let size = size.min(xattr::XATTR_SIZE_MAX);
    if size > 0 && !validate_user_buf(val, size) { return -14; }
    let mut kval: Vec<u8> = Vec::new();
    if kval.try_reserve_exact(size).is_err() { return -12; }
    kval.resize(size, 0);

    let pid = current_pid();
    let kp;
    let arg0 = if is_fd {
        target as u64
    } else {
        kp = match resolve_user_path(target) { Ok(p) => p, Err(e) => return e };
        kp.ptr() as u64
    };
    let tag = if is_fd {
        vfs::VFS_FGETXATTR
    } else if nofollow {
        vfs::VFS_LGETXATTR
    } else {
        vfs::VFS_GETXATTR
    };
    let msg = make_vfs_msg(tag, &[arg0, kname.as_ptr() as u64, kval.as_mut_ptr() as u64, size as u64]);
    let r = vfs_reply_val(&vfs::handle(&msg, pid));
    copy_out_bounce(r, size, val, &kval)
}

/// listxattr(path, list, size) / llistxattr / flistxattr → total bytes of
/// NUL-joined names or -errno. `size == 0` is a length query, same as
/// getxattr above.
fn sys_listxattr(target: usize, list: usize, size: usize, is_fd: bool, nofollow: bool) -> isize {
    let size = size.min(xattr::XATTR_SIZE_MAX);
    if size > 0 && !validate_user_buf(list, size) { return -14; } // EFAULT
    let mut klist: Vec<u8> = Vec::new();
    if klist.try_reserve_exact(size).is_err() { return -12; }
    klist.resize(size, 0);

    let pid = current_pid();
    let kp;
    let arg0 = if is_fd {
        target as u64
    } else {
        kp = match resolve_user_path(target) { Ok(p) => p, Err(e) => return e };
        kp.ptr() as u64
    };
    let tag = if is_fd {
        vfs::VFS_FLISTXATTR
    } else if nofollow {
        vfs::VFS_LLISTXATTR
    } else {
        vfs::VFS_LISTXATTR
    };
    let msg = make_vfs_msg(tag, &[arg0, klist.as_mut_ptr() as u64, size as u64]);
    let r = vfs_reply_val(&vfs::handle(&msg, pid));
    copy_out_bounce(r, size, list, &klist)
}

/// removexattr(path, name) / lremovexattr / fremovexattr → 0 or -errno.
fn sys_removexattr(target: usize, name: usize, is_fd: bool, nofollow: bool) -> isize {
    let kname = match copy_xattr_name(name) { Ok(n) => n, Err(e) => return e };

    let pid = current_pid();
    let kp;
    let arg0 = if is_fd {
        target as u64
    } else {
        kp = match resolve_user_path(target) { Ok(p) => p, Err(e) => return e };
        kp.ptr() as u64
    };
    let tag = if is_fd {
        vfs::VFS_FREMOVEXATTR
    } else if nofollow {
        vfs::VFS_LREMOVEXATTR
    } else {
        vfs::VFS_REMOVEXATTR
    };
    let msg = make_vfs_msg(tag, &[arg0, kname.as_ptr() as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_fchown(fd: usize, uid: usize, gid: usize) -> isize {
    if !chown_ids_mapped(uid, gid) { return -22; } // EINVAL: unmapped in the caller's userns
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_FCHOWN, &[fd as u64, uid as u64, gid as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// chown family ids are taken in the caller's namespace view; an id with no
/// mapping there is EINVAL (-1 = leave unchanged).
fn chown_ids_mapped(uid: usize, gid: usize) -> bool {
    sched::ns::ids_mapped(0, &[uid as u32]) && sched::ns::ids_mapped(1, &[gid as u32])
}

fn sys_fchownat(dirfd: usize, path_ptr: usize, uid: usize, gid: usize, flags: usize) -> isize {
    if !chown_ids_mapped(uid, gid) { return -22; }
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let tag = if flags & AT_SYMLINK_NOFOLLOW != 0 { vfs::VFS_LCHOWN } else { vfs::VFS_CHOWN };
    let msg = make_vfs_msg(tag, &[path.ptr() as u64, uid as u64, gid as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// setgroups(size, list) — replace the supplementary group list. Root only
/// (EPERM), at most `NGROUPS_MAX` entries (EINVAL). Inherited across fork and
/// exec like the other credentials; consulted by every filesystem permission
/// check through `xattr::Cred::in_group`.
fn sys_setgroups(size: usize, list_ptr: usize) -> isize {
    if sched::ns::setgroups_denied(sched::ns::current_userns()) { return -1; } // EPERM after "deny"
    if size > xattr::NGROUPS_MAX { return -22; } // EINVAL
    let mut groups = [0u32; xattr::NGROUPS_MAX];
    if size > 0 {
        if !validate_user_buf(list_ptr, size * 4) { return -14; } // EFAULT
        prefault_user(list_ptr, size * 4);
        let bytes = unsafe { core::slice::from_raw_parts_mut(groups.as_mut_ptr() as *mut u8, size * 4) };
        let ok = with_current_address_space(|as_| as_.read_user_buf(list_ptr, bytes)).unwrap_or(false);
        if !ok { return -14; }
    }
    if sched::set_current_groups(&groups[..size]) { 0 } else { -1 } // EPERM
}

/// getgroups(size, list) — `size == 0` queries the count; otherwise the list
/// is copied out (EINVAL when it does not fit). Returns the count.
fn sys_getgroups(size: usize, list_ptr: usize) -> isize {
    let mut groups = [0u32; xattr::NGROUPS_MAX];
    let n = sched::groups_of(current_pid(), &mut groups);
    if size == 0 { return n as isize; }
    if size < n { return -22; } // EINVAL
    if n > 0 {
        if !validate_user_buf(list_ptr, n * 4) { return -14; }
        prefault_user(list_ptr, n * 4);
        let bytes = unsafe { core::slice::from_raw_parts(groups.as_ptr() as *const u8, n * 4) };
        let ok = copy_to_user_bytes(list_ptr, bytes);
        if !ok { return -14; }
    }
    n as isize
}

/// One resolved timestamp for the VFS: `UTIME_NOW` becomes the current time
/// here (so both filesystems stamp the same clock userspace reads), and
/// `UTIME_OMIT` travels as `nsec == UTIME_OMIT`. Returns `(sec, nsec,
/// explicit)` where `explicit` means the caller supplied a concrete time.
fn utimens_resolve(sec: i64, nsec: i64, now: (i64, i64)) -> Result<(i64, i64, bool), isize> {
    if nsec == vfs::UTIME_NOW {
        Ok((now.0, now.1, false))
    } else if nsec == vfs::UTIME_OMIT {
        Ok((0, vfs::UTIME_OMIT, false))
    } else if !(0..1_000_000_000).contains(&nsec) {
        Err(-22) // EINVAL
    } else {
        Ok((sec, nsec, true))
    }
}

/// Send a resolved `VFS_UTIMENS`/`VFS_LUTIMENS`/`VFS_FUTIMENS`.
fn utimens_send(tag: u64, target: u64, at: (i64, i64, bool), mt: (i64, i64, bool)) -> isize {
    let mut flags = 0u64;
    if at.2 || mt.2 { flags |= vfs::UTIMENS_EXPLICIT; }
    let pid = current_pid();
    let msg = make_vfs_msg(tag, &[target, at.0 as u64, at.1 as u64, mt.0 as u64, mt.1 as u64, flags]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// utimensat(dirfd, path, times, flags) — also `futimens` (path == NULL, the
/// fd form). `times == NULL` means both timestamps become the current time.
fn sys_utimensat(dirfd: usize, path_ptr: usize, times_ptr: usize, flags: usize) -> isize {
    // struct timespec[2]: { atime.sec, atime.nsec, mtime.sec, mtime.nsec }
    let mut ts = [0i64; 4];
    if times_ptr != 0 {
        if !validate_user_buf(times_ptr, 32) { return -14; }
        prefault_user(times_ptr, 32);
        let bytes = unsafe { core::slice::from_raw_parts_mut(ts.as_mut_ptr() as *mut u8, 32) };
        let ok = with_current_address_space(|as_| as_.read_user_buf(times_ptr, bytes)).unwrap_or(false);
        if !ok { return -14; }
    } else {
        ts = [0, vfs::UTIME_NOW, 0, vfs::UTIME_NOW];
    }
    // One reading for both, so "now" means the same instant for atime and mtime.
    let now = sched::clock_ts();
    let at = match utimens_resolve(ts[0], ts[1], now) { Ok(v) => v, Err(e) => return e };
    let mt = match utimens_resolve(ts[2], ts[3], now) { Ok(v) => v, Err(e) => return e };
    if at.1 == vfs::UTIME_OMIT && mt.1 == vfs::UTIME_OMIT { return 0; } // nothing to do (Linux skips the lookup too)
    if path_ptr == 0 {
        // futimens(fd, times): Linux accepts a NULL path here (glibc/musl
        // both use it) and acts on the descriptor.
        if dirfd == AT_FDCWD { return -14; } // EFAULT, as Linux
        return utimens_send(vfs::VFS_FUTIMENS, dirfd as u64, at, mt);
    }
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let tag = if flags & AT_SYMLINK_NOFOLLOW != 0 { vfs::VFS_LUTIMENS } else { vfs::VFS_UTIMENS };
    utimens_send(tag, path.ptr() as u64, at, mt)
}

/// x86-64 legacy `utimes(path, timeval[2])` / `futimesat(dirfd, path,
/// timeval[2])` and — with `is_utime` — `utime(path, utimbuf)` (two `time_t`
/// seconds). NULL times = now.
#[cfg(not(target_arch = "aarch64"))]
fn sys_utimes(dirfd: usize, path_ptr: usize, times_ptr: usize, is_utime: bool) -> isize {
    let mut ts = [0i64; 4];
    if times_ptr != 0 {
        let n = if is_utime { 16 } else { 32 };
        if !validate_user_buf(times_ptr, n) { return -14; }
        prefault_user(times_ptr, n);
        let mut raw = [0i64; 4];
        let bytes = unsafe { core::slice::from_raw_parts_mut(raw.as_mut_ptr() as *mut u8, n) };
        let ok = with_current_address_space(|as_| as_.read_user_buf(times_ptr, bytes)).unwrap_or(false);
        if !ok { return -14; }
        if is_utime {
            ts = [raw[0], 0, raw[1], 0];
        } else {
            if !(0..1_000_000).contains(&raw[1]) || !(0..1_000_000).contains(&raw[3]) { return -22; }
            ts = [raw[0], raw[1] * 1000, raw[2], raw[3] * 1000];
        }
    } else {
        ts = [0, vfs::UTIME_NOW, 0, vfs::UTIME_NOW];
    }
    // One reading for both, so "now" means the same instant for atime and mtime.
    let now = sched::clock_ts();
    let at = match utimens_resolve(ts[0], ts[1], now) { Ok(v) => v, Err(e) => return e };
    let mt = match utimens_resolve(ts[2], ts[3], now) { Ok(v) => v, Err(e) => return e };
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    utimens_send(vfs::VFS_UTIMENS, path.ptr() as u64, at, mt)
}

fn sys_flock(fd: usize, op: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_FLOCK, &[fd as u64, op as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// sys_pipe2(pipefd, flags) — create a pipe.
///
/// `flags` (O_CLOEXEC / O_NONBLOCK) must reach the VFS. Discarding it meant no
/// pipe end was ever close-on-exec, so every pipe the shell held leaked into
/// each child it exec'd; with a stray copy of the write end alive in the child,
/// the read end never reached EOF and the reader blocked forever. std's
/// `io::pipe()` — which brush uses for both pipelines and `$(...)` — always
/// passes O_CLOEXEC.
fn sys_pipe2(pipefd_ptr: usize, flags: usize) -> isize {
    // int pipefd[2] — two ints (4 bytes each) packed at pipefd_ptr. The VFS
    // stores both fds with FD_TABLES held, so it is handed a kernel buffer;
    // the pair is copied out here, with nothing held. A bad pointer is EFAULT
    // and both new ends are closed again, as on Linux.
    if pipefd_ptr == 0 || !validate_user_buf(pipefd_ptr, 8) { return -14; }
    let mut fds = [0u32; 2];
    let rfd_ptr = fds.as_mut_ptr() as usize;
    let wfd_ptr = rfd_ptr + 4;
    let pid = current_pid();
    // pipe2's O_DIRECT is one of the bits whose value differs on aarch64.
    let msg = make_vfs_msg(vfs::VFS_PIPE, &[rfd_ptr as u64, wfd_ptr as u64, open_flags_from_user(flags) as u64]);
    let r = vfs_reply_val(&vfs::handle(&msg, pid));
    if r >= 0 && !sched::uaccess::write_user(pipefd_ptr, fds) {
        sys_close(fds[0] as usize);
        sys_close(fds[1] as usize);
        return -14;
    }
    if SYSCALL_TRACE_FDS && r >= 0 {
        trace_fd("pipe2 r/w", fds[0] as usize, fds[1] as usize, flags, r);
    } else {
        trace_fd("pipe2", 0, 0, flags, r);
    }
    r
}

/// What user descriptor `fd` names, for routing: the socket or epoll handle
/// behind a socket/epoll descriptor (`vfs::VnodeKind::SockAlias` /
/// `EpollFd`), the number itself for every other descriptor, and
/// `vfs::BAD_FD` for a raw handle number (handles are not descriptors). One
/// fd-table lookup; the per-kind code below routes by the handle's range.
#[inline]
fn fd_handle(fd: usize) -> usize {
    vfs::resolve_user_fd(current_pid(), fd)
}

/// Is `h` (a translated descriptor) a socket handle?
#[inline]
fn is_sock_handle(h: usize) -> bool { (net_server::SOCK_FD_BASE..net_server::SOCK_FD_END).contains(&h) }

/// Is `h` (a translated descriptor) an epoll handle?
#[inline]
fn is_epoll_handle(h: usize) -> bool { (EPOLL_FD_BASE..EPOLL_FD_BASE + MAX_EPOLL_FDS).contains(&h) }

/// Translate the fd arguments of the syscalls that take a descriptor and act
/// on what it names (I/O, socket calls, fcntl/ioctl/fstat, epoll) into the
/// handle they name, once, at the boundary. close, dup and dup2/dup3 are not
/// translated here: they act on the descriptor number itself.
/// poll/select readiness is translated per fd in the probe functions.
fn fd_handle_args(number: usize, a0: usize, a2: usize) -> (usize, usize) {
    match number {
        READ | WRITE | READV | WRITEV | PREAD64 | PWRITE64 | PREADV | PWRITEV | PREADV2
        | PWRITEV2 | FSTAT | LSEEK | IOCTL
        | FCNTL | FLOCK | FSYNC | FDATASYNC | FTRUNCATE | FSTATFS | FCHMOD | FCHOWN
        | NEWFSTATAT | STATX | SENDFILE | FALLOCATE
        | BIND | LISTEN | ACCEPT | ACCEPT4 | CONNECT | SENDTO | RECVFROM | SENDMSG
        | RECVMSG | SENDMMSG | RECVMMSG | SHUTDOWN | GETSOCKNAME | GETPEERNAME | SETSOCKOPT | GETSOCKOPT
        | EPOLL_PWAIT | EPOLL_PWAIT2 | VMSPLICE
            => (fd_handle(a0), a2),
        // splice(fd_in, off_in, fd_out, ...): either end may be a socket.
        EPOLL_CTL | SPLICE => (fd_handle(a0), fd_handle(a2)),
        #[cfg(not(target_arch = "aarch64"))]
        EPOLL_WAIT => (fd_handle(a0), a2),
        _ => (a0, a2),
    }
}

/// Give a freshly created or duplicated socket/epoll handle `h` a descriptor
/// number (the lowest free one `>= min`). On failure (EMFILE, or EINVAL for
/// a `min` past the limit) the handle is closed, so the caller can simply
/// return the result.
fn publish_handle(pid: u32, h: usize, min: usize) -> isize {
    let fd = vfs::install_handle(pid, h, min);
    if fd < 0 { close_handle(pid, h); }
    fd
}

/// Close a socket or epoll handle that no descriptor names (any more).
fn close_handle(pid: u32, h: usize) {
    if is_epoll_handle(h) {
        let _ = sys_epoll_close(h);
    } else if is_sock_handle(h) {
        close_sock_alias_target(pid, h);
    }
}

/// Is handle `h` still open in thread group `tgid` (after an exec sweep or a
/// fork that did not copy it)?
fn handle_alive(tgid: u32, h: usize) -> bool {
    if is_epoll_handle(h) { epoll_slot_for(h, tgid).is_some() }
    else if is_sock_handle(h) { net_server::sock_is_open(tgid, h) }
    else { true }
}

/// Device of every socket's inode (Linux's sockfs is an anonymous
/// superblock; `makedev(0, 8)` is what a typical system shows).
const SOCKFS_DEV: u64 = 8;
/// Device and inode of the one anonymous inode every epoll fd names (Linux's
/// `anon_inode_getfd` shares a single inode among all of them).
const ANON_INODE_DEV: u64 = 13;
const ANON_INODE_INO: u64 = 0x5000_0000;

/// `(st_mode, st_ino, st_dev)` for fstat of socket or epoll handle `h`, when
/// `pid`'s process holds it. A socket is `S_IFSOCK | 0777` whose inode number
/// is its open file description id — the number `/proc/net/*` lists and
/// `readlink("/proc/self/fd/N")` shows as `socket:[ino]`, shared by its dups
/// and fork copies as a socket inode is. An epoll fd is an anonymous inode:
/// mode 0600 with no file-type bits (`anon_inode:[eventpoll]`).
fn handle_stat(pid: u32, h: usize) -> Option<(u32, u64, u64)> {
    const S_IFSOCK: u32 = 0o140000;
    if is_epoll_handle(h) {
        return epoll_slot_for(h, sched::tgid_of(pid)).map(|_| (0o600, ANON_INODE_INO, ANON_INODE_DEV));
    }
    if is_sock_handle(h) {
        return net_server::fd_ofd(pid, h).map(|ofd| (S_IFSOCK | 0o777, ofd as u64, SOCKFS_DEV));
    }
    None
}

/// A socket call (send/recv/bind/...) on a descriptor in the VFS range — a
/// pipe, a file, an eventfd — or on an epoll fd, after alias translation: ENOTSOCK when that fd
/// is open, EBADF when it is not, as on Linux. The net server only knows its
/// own socket range and answered EBADF for both, and some callers branch on
/// the difference: libpulse's `pa_write` first tries `send(fd, MSG_NOSIGNAL)`
/// and falls back to `write` only on ENOTSOCK, so on EBADF its threaded
/// mainloop could never wake itself through its wakeup pipe ("pa_write()
/// failed while trying to wake up the mainloop: Bad file descriptor") and
/// Firefox's PulseAudio stream never started.
fn not_a_socket(number: usize, fd: usize) -> Option<isize> {
    match number {
        BIND | LISTEN | ACCEPT | ACCEPT4 | CONNECT | SENDTO | RECVFROM | SENDMSG
        | RECVMSG | SENDMMSG | RECVMMSG | SHUTDOWN | GETSOCKNAME | GETPEERNAME | SETSOCKOPT | GETSOCKOPT => {}
        _ => return None,
    }
    // Epoll fds live above the socket range, in their own table: an open one
    // is a non-socket too, a closed (or never-held) number is EBADF.
    if is_epoll_handle(fd) {
        return Some(if epoll_slot_for(fd, sched::current_tgid()).is_some() { -88 } else { -9 });
    }
    if is_sock_handle(fd) { return None; }
    Some(if vfs::fd_ofd(current_pid(), fd).is_some() { -88 } else { -9 }) // ENOTSOCK / EBADF
}

fn sys_dup(oldfd: usize) -> isize {
    let pid = current_pid();
    // A socket or epoll fd duplicates as a new handle on the same object,
    // published at the lowest free number.
    let h = fd_handle(oldfd);
    if is_sock_handle(h) || is_epoll_handle(h) {
        let d = dup_handle(pid, h, false);
        if d < 0 { return d; }
        return publish_handle(pid, d as usize, 0);
    }
    // dup() picks the lowest free fd — that's VFS_ALLOC_FD. (VFS_DUP2 targets a
    // specific newfd and rejects the u64::MAX "any" sentinel as out of range.)
    let msg = make_vfs_msg(vfs::VFS_ALLOC_FD, &[oldfd as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// A second handle on the object socket/epoll handle `h` names, with its own
/// FD_CLOEXEC (`cloexec`). The caller gives it a descriptor (or closes it).
fn dup_handle(pid: u32, h: usize, cloexec: bool) -> isize {
    const F_DUPFD: usize = 0;
    const F_DUPFD_CLOEXEC: usize = 1030;
    if is_epoll_handle(h) {
        return epoll_fcntl(h, if cloexec { F_DUPFD_CLOEXEC } else { F_DUPFD }, 0);
    }
    let msg = make_vfs_msg(net_server::NET_DUP, &[h as u64, cloexec as u64, 0]);
    net_reply_val(&net_server::handle(&msg, pid))
}

/// dup3(oldfd, newfd, flags) — also the implementation of dup2 (flags = 0).
///
/// `flags` (only O_CLOEXEC is defined) must reach the VFS: it is the sole
/// source of the new descriptor's close-on-exec state. Dropping it left
/// handle_dup2 inheriting oldfd's flag instead, so dup2'ing an O_CLOEXEC
/// descriptor onto a child's stdio — exactly what posix_spawn's file actions do
/// for every shell redirection — produced a redirection that execve then
/// immediately closed.
fn sys_dup3(oldfd: usize, newfd: usize, flags: usize) -> isize {
    let pid = current_pid();
    const O_CLOEXEC: usize = 0x8_0000;
    if newfd != usize::MAX {
        let old = fd_handle(oldfd);
        if oldfd == newfd {
            // dup2(fd, fd) is a no-op on an open fd; let the VFS answer it
            // (it also knows the untracked console fds).
        } else if is_sock_handle(old) || is_epoll_handle(old) {
            // A socket or epoll fd onto `newfd`: a new handle on the same
            // object, installed there. (Firefox's child launch: dup2(ipc_socket,
            // 3), then execve; a failed dup2 there is `_exit(127)`.)
            if newfd >= vfs::fd_limit(pid) { return -9; } // EBADF: past RLIMIT_NOFILE
            let cloexec = flags & O_CLOEXEC != 0;
            let dup = dup_handle(pid, old, cloexec);
            if dup < 0 { return dup; }
            // Whatever sat on newfd goes, as dup2 does: a socket or epoll fd
            // with its handle, an ordinary file with its epoll registrations.
            let target = fd_handle(newfd);
            if target != newfd { close_handle(pid, target); }
            else if vfs::vfs_get_node_kind(pid, newfd).is_some() {
                if !fd_private() { epoll_release_fd(pid, sched::current_tgid(), newfd); }
            }
            let r = vfs::install_handle_at(pid, newfd, dup as usize);
            if r < 0 { close_handle(pid, dup as usize); }
            trace_fd("dup3 handle", oldfd, newfd, flags, r);
            return r;
        } else if vfs::vfs_get_node_kind(pid, oldfd).is_some()
            || (oldfd <= 2 && !vfs::fd_redirected(pid, oldfd)) {
            // Overwriting a socket or epoll fd with an ordinary one: its
            // handle goes too.
            let target = fd_handle(newfd);
            if target != newfd && newfd < vfs::fd_limit(pid) { close_handle(pid, target); }
        }
    }
    // If newfd == u64::MAX this is sys_dup (allocate any free fd).
    let tag = if newfd == usize::MAX { vfs::VFS_ALLOC_FD } else { vfs::VFS_DUP2 };
    // dup2/dup3 onto an open newfd closes the file that was there: its epoll
    // registrations go with it, before the number is reused (Linux keys them
    // by file, and the file that lands on newfd was never registered).
    if newfd != usize::MAX && newfd != oldfd && vfs::vfs_get_node_kind(pid, oldfd).is_some() {
        if !fd_private() { epoll_release_fd(pid, sched::current_tgid(), newfd); }
    }
    let msg = make_vfs_msg(tag, &[oldfd as u64, newfd as u64, flags as u64]);
    let r = vfs_reply_val(&vfs::handle(&msg, pid));
    trace_fd("dup3", oldfd, newfd, flags, r);
    r
}

fn sys_getdents64(fd: usize, buf_ptr: usize, count: usize) -> isize {
    // The VFS and the mount servers fill the dirent buffer with their own
    // locks held (the fd table, TMP_FILES, the f2fs mount). A user buffer
    // there turned a bad pointer into a kernel-mode fault under those locks:
    // the caller was killed with them held and every later directory op
    // wedged. They fill a kernel bounce buffer instead, and the result is
    // copied out here, with nothing held, through the fault-tolerant copy.
    // Capped: getdents may return fewer entries than fit, and the cursor
    // picks up where this call stopped.
    const BOUNCE_MAX: usize = 64 * 1024;
    if buf_ptr == 0 || !validate_user_buf(buf_ptr, count) { return -14; }
    let n = count.min(BOUNCE_MAX);
    let mut kbuf: Vec<u8> = Vec::new();
    if kbuf.try_reserve_exact(n).is_err() { return -12; }
    kbuf.resize(n, 0);
    // The VFS advances the directory cursor as it fills the bounce buffer, so
    // a copy-out fault afterwards would lose the entries. Probe that every
    // page of the destination is writable first (read a byte, store it back)
    // and fail with EFAULT before any cursor moves.
    {
        let end = buf_ptr + n;
        let mut a = buf_ptr;
        while a < end {
            let mut b = 0u8;
            if unsafe { sched::uaccess::copy_from_user(&mut b, a, 1) } != 0
                || unsafe { sched::uaccess::copy_to_user(a, &b, 1) } != 0 { return -14; }
            a = (a & !4095) + 4096;
        }
    }
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_GETDENTS64, &[fd as u64, kbuf.as_mut_ptr() as u64, n as u64]);
    let r = vfs_reply_val(&vfs::handle(&msg, pid));
    if r <= 0 { return r; }
    let len = (r as usize).min(n);
    if unsafe { sched::uaccess::copy_to_user(buf_ptr, kbuf.as_ptr(), len) } != 0 { return -14; }
    len as isize
}

fn sys_mkdirat(dirfd: usize, path_ptr: usize, mode: usize) -> isize {
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_MKDIR, &[path.ptr() as u64, pack_create_mode(mode)]);
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
    // chdir("/proc/self/fd/N") follows the magic link, as on Linux: it is
    // fchdir(N), not a cwd inside /proc.
    if let Some((fd, b"")) = proc_own_fd(path.bytes()) { return sys_fchdir(fd); }
    // chdir("/proc/<pid>/cwd|root") follows the magic link; another user's is EACCES.
    // The link target is another live process's cwd/root, reached by the magic
    // jump rather than a walk, so it needs no search-permission probe.
    let magic = proc_magic(path.bytes(), b"cwd").map_or(false, |(_, r)| r.is_empty())
        || proc_magic(path.bytes(), b"root").map_or(false, |(_, r)| r.is_empty());
    let path = match follow_magic_final(path) { Ok(p) => p, Err(e) => return e };
    if magic { sched::set_cwd(path.ns_bytes()); return 0; }

    // Check the target exists by opening it O_RDONLY. Probe the *resolved*
    // path: re-resolving the user pointer here would repeat the work, and
    // would drift if the user buffer changed under us in between.
    let pid = current_pid();
    let omsg = make_vfs_msg(vfs::VFS_OPEN, &[path.ptr() as u64, 0u64, 0]);
    let fd = vfs_reply_val(&vfs::handle(&omsg, pid));
    if fd < 0 { return fd; }
    sys_close(fd as usize);

    sched::set_cwd(path.ns_bytes());
    0
}

/// fchdir(fd) — make the directory `fd` names the new cwd.
///
/// This used to be a hardcoded ENOTDIR ("no directory fds yet"). Directory
/// descriptors do work — `resolve_at_path` resolves against them — so recover
/// the fd's absolute path the same way and adopt it, after confirming it
/// really is a directory (fchdir on a regular file is ENOTDIR, as on Linux).
fn sys_fchdir(fd: usize) -> isize {
    let base = match fd_abs_path(fd) { Some(b) => b, None => return -9 }; // EBADF
    // fd_abs_path answers in the caller's chrooted view ("/" for the jail
    // root), but the cwd is stored host-absolute: re-anchor it under the
    // jail, or a jailed fchdir(open("/")) put the cwd at the REAL root -- a
    // jail escape, and why apk's fchdir(root_fd); chroot(".") inside an
    // Alpine chroot ran its package scripts against the host. Unjailed this
    // is the same normalised absolute path.
    let kp = match resolve_ns(&base).and_then(KPath::from_ns) { Ok(k) => k, Err(e) => return e };

    const S_IFMT: u32 = 0o170000;
    const S_IFDIR: u32 = 0o040000;
    let mut stat_buf = [0u8; STAT_SIZE];
    let msg = make_vfs_msg(vfs::VFS_STAT, &[kp.ptr() as u64, stat_buf.as_mut_ptr() as u64]);
    if vfs_reply_val(&vfs::handle(&msg, current_pid())) < 0 { return -9; } // EBADF
    if vfs::read_stat_mode(stat_buf.as_ptr() as usize) & S_IFMT != S_IFDIR { return -20; } // ENOTDIR

    sched::set_cwd(kp.ns_bytes());
    0
}

fn sys_getcwd(buf_ptr: usize, size: usize) -> isize {
    if !validate_user_buf(buf_ptr, size.min(1)) { return -14; }
    let cwd = match sched::current_cwd_arc() { Some(c) => c, None => return -34 };
    // Strip the jail root so a chrooted process sees itself at "/". The cwd is
    // stored host-absolute and always under the root, so this is a prefix
    // trim; when the cwd *is* the root it reads as "/".
    let view = jail_relative(&cwd);
    let n = view.len();
    // Linux: a cwd longer than PATH_MAX cannot be returned at all.
    if n + 1 > KPATH_MAX { return -36; } // ENAMETOOLONG
    if n >= size { return -34; } // ERANGE
    let mut out = view;
    out.push(0);
    if unsafe { sched::uaccess::copy_to_user(buf_ptr, out.as_ptr(), n + 1) } != 0 { return -14; }
    (n + 1) as isize
}

/// chroot(path) — confine the calling process to `path` as its filesystem root.
///
/// Requires euid 0, like Linux. `path` is resolved through the normal machinery,
/// so a chroot issued *inside* an existing jail composes correctly (the new
/// root is itself confined to the old one). The target must be a directory.
///
/// POSIX leaves cwd unchanged across chroot, but a cwd left pointing outside the
/// new root is the classic escape (`chdir("..")` walks straight out). Since the
/// cwd is stored host-absolute, we reset it to the new root unless it already
/// lies within — closing that hole while still satisfying "cwd is somewhere
/// under the root".
fn sys_chroot(path_ptr: usize) -> isize {
    if !sched::caps::capable(sched::caps::CAP_SYS_CHROOT) { return -1; } // EPERM
    let path = match resolve_user_path(path_ptr) { Ok(p) => p, Err(e) => return e };

    // Must exist and be a directory.
    const S_IFMT: u32 = 0o170000;
    const S_IFDIR: u32 = 0o040000;
    let mut stat_buf = [0u8; STAT_SIZE];
    let smsg = make_vfs_msg(vfs::VFS_STAT, &[path.ptr() as u64, stat_buf.as_mut_ptr() as u64]);
    if vfs_reply_val(&vfs::handle(&smsg, current_pid())) < 0 { return -2; } // ENOENT
    if vfs::read_stat_mode(stat_buf.as_ptr() as usize) & S_IFMT != S_IFDIR { return -20; } // ENOTDIR

    // A procfs directory is never a usable root: the VFS matches /proc files
    // by suffix, so a root joined as text under /proc would leak
    // /proc/self/status. Root into the empty read-only void directory instead
    // (Firefox's chroot("/proc/self/fdinfo") wants a root that holds nothing).
    let ns = path.ns_bytes();
    let in_proc = ns == b"/proc" || ns.starts_with(b"/proc/");
    let new_root: &[u8] = if in_proc { vfs::VOID_ROOT } else { ns };
    // Decide whether the current cwd survives: keep it only if it is at or below
    // the new root, otherwise re-anchor at the root.
    let cwd_under_root = match sched::current_cwd_arc() {
        Some(c) => vfs::alias::under(&c, new_root),
        None => false,
    };

    if !sched::set_root(new_root) { return -3; } // ESRCH
    if !cwd_under_root {
        sched::set_cwd(new_root);
    }
    0
}

fn sys_setpgid(pid_raw: usize, pgid_raw: usize) -> isize {
    // Linux rules (own process or own child, same session, no session
    // leaders, target group in the caller's session) — see sched::cred.
    // Negative values stay negative (EINVAL); nonzero ids are translated.
    let (mut pid, mut pgid) = (pid_raw as u32 as i32, pgid_raw as u32 as i32);
    if pid > 0 { match sched::pidns::arg(pid as u32) { Ok(k) => pid = k as i32, Err(e) => return e } }
    if pgid > 0 { match sched::pidns::arg(pgid as u32) { Ok(k) => pgid = k as i32, Err(e) => return e } }
    sched::cred::set_pgid_checked(pid, pgid)
}

fn sys_getpgid(pid_raw: usize) -> isize {
    if pid_raw == 0 { return sched::pidns::out(sched::current_pgid()) as isize; }
    let pid_raw = match sched::pidns::arg(pid_raw as u32) { Ok(k) => k as usize, Err(e) => return e };
    // Honour the argument. Returning the *caller's* pgid for every pid made
    // getpgid(other) silently wrong, which job control cannot survive: a shell
    // uses it to decide whether a child ever made it into the group it was
    // placed in, and "yes, always" is the one answer that hides the failure.
    match sched::pgid_of(pid_raw as u32) {
        Some(pgid) => sched::pidns::out(pgid) as isize,
        None       => -3, // ESRCH
    }
}

fn sys_getresxid(r_ptr: usize, e_ptr: usize, s_ptr: usize, is_gid: bool) -> isize {
    let (r, e, s) = if is_gid {
        (sched::current_gid(), sched::current_egid(), sched::current_sgid())
    } else {
        (sched::current_uid(), sched::current_euid(), sched::current_suid())
    };
    let k = is_gid as usize;
    let (r, e, s) = (sched::ns::view_id(k, r), sched::ns::view_id(k, e), sched::ns::view_id(k, s));
    if r_ptr != 0 && !ua::write_user::<u32>(r_ptr, r) { return -14; }
    if e_ptr != 0 && !ua::write_user::<u32>(e_ptr, e) { return -14; }
    if s_ptr != 0 && !ua::write_user::<u32>(s_ptr, s) { return -14; }
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
/// Route through VFS_ACCESS instead, which lets the owning filesystem answer
/// via `xattr::access_check` so stored POSIX ACLs are honored, not just raw
/// mode bits. A filesystem that doesn't implement it yet replies -ENOSYS, in
/// which case fall back to the old VFS_STAT + mode-bit probe below (kept
/// verbatim, F_OK short-circuit and RamFS/tmpfs fallback included) so nothing
/// regresses for a mount that predates VFS_ACCESS.
fn sys_faccessat(dirfd: usize, path_ptr: usize, mode: usize, flags: usize) -> isize {
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    // access(2) checks what a final magic link (`/proc/self/exe`) names,
    // unless AT_SYMLINK_NOFOLLOW asks for the link itself.
    let path = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match follow_magic_final(path) { Ok(p) => p, Err(e) => return e }
    } else { path };
    let path_ptr = path.ptr();

    const F_OK: usize = 0;
    const X_OK: usize = 1;
    const W_OK: usize = 2;
    const R_OK: usize = 4;
    // Linux `AT_EACCESS`: check with the effective ids instead of the real
    // ones. Without it — plain `access(2)` always calls in with flags == 0 —
    // POSIX asks for the REAL uid/gid, so a setuid-root program can find out
    // what its (often unprivileged) invoker is allowed to do, not what it
    // itself can do right now.
    const AT_EACCESS: usize = 0x200;

    let pid = current_pid();
    let eaccess = flags & AT_EACCESS != 0;

    let amsg = make_vfs_msg(vfs::VFS_ACCESS, &[path_ptr as u64, mode as u64, eaccess as u64]);
    let ar = vfs_reply_val(&vfs::handle(&amsg, pid));
    if ar != -38 { return ar; } // anything but ENOSYS is the real answer

    // Legacy fallback: VFS_STAT then answer R_OK/W_OK/X_OK from the raw mode
    // bits, for a filesystem that doesn't implement VFS_ACCESS yet.
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
    let path = kpath.bytes();

    // The magic links. Each answers with a path as the CALLER sees it (its
    // chroot stripped, alias mounts mapped back), as Linux's d_path does
    // relative to the reader's root.
    let reply = |bytes: &[u8]| -> isize {
        let n = bytes.len().min(size);
        if unsafe { sched::uaccess::copy_to_user(buf_ptr, bytes.as_ptr(), n) } != 0 { return -14; }
        n as isize
    };

    // /proc/<pid>/exe → the real path that process was execve'd from (sched's
    // tgid-keyed side table). A numeric pid is translated to its tgid so a
    // thread named by any of its siblings' pids still hits the one shared
    // entry. Falls back to "/bin/init" only when the pid is live but unset —
    // correct for the boot-loaded PID1, which never goes through sys_execve.
    // A pid naming no live task is -ENOENT.
    if let Some((pid, rest)) = proc_magic(path, b"exe") {
        if rest.is_empty() {
            if sched::exists_probe(pid) != 0 { return -2; }
            if !proc_link_allowed(pid) { return -13; }
            // A leaderless group (leader pthread_exited) has no exe, as on Linux.
            if sched::leaderless(sched::tgid_of(pid)) { return -2; }
            let mut kb: Vec<u8> = alloc::vec![0u8; HOST_PATH_MAX];
            return match sched::exe_path(sched::tgid_of(pid), &mut kb) {
                Some(len) => reply(&host_to_view(&kb[..len])),
                None      => reply(b"/bin/init"),
            };
        }
    }
    // /proc/<pid>/cwd and /proc/<pid>/root.
    if let Some((pid, rest)) = proc_magic(path, b"cwd") {
        if rest.is_empty() {
            if sched::exists_probe(pid) == 0 && !proc_link_allowed(pid) { return -13; }
            return match sched::cwd_arc_of(pid) {
                Some(c) => reply(&jail_relative(&c)),
                None => -2,
            };
        }
    }
    if let Some((pid, k)) = proc_ns_link(path) {
        if sched::exists_probe(pid) != 0 { return -2; }
        if !proc_link_allowed(pid) { return -13; }
        let mut b = [0u8; 48];
        let name = sched::ns::NS_NAMES[k];
        let mut n = name.len();
        b[..n].copy_from_slice(name);
        b[n] = b':'; b[n + 1] = b'['; n += 2;
        let mut d = [0u8; 20]; let mut j = 0;
        let mut v = sched::ns::ns_ino(k, sched::ns::ns_id_of(pid, k));
        loop { d[j] = b'0' + (v % 10) as u8; j += 1; v /= 10; if v == 0 { break; } }
        while j > 0 { j -= 1; b[n] = d[j]; n += 1; }
        b[n] = b']'; n += 1;
        return reply(&b[..n]);
    }
    if let Some((pid, rest)) = proc_magic(path, b"root") {
        if rest.is_empty() {
            if sched::exists_probe(pid) != 0 { return -2; }
            if !proc_link_allowed(pid) { return -13; }
            return match sched::root_arc_of(pid) {
                Some(r) => reply(&jail_relative(&r)),
                None => reply(&jail_relative(b"/")),
            };
        }
    }

    // /proc/<pid>/fd/N → resolve fd N in that process's table via VFS. Another
    // process needs the ptrace-style check (same euid, or root); a leaderless
    // group has no table (ENOENT), as on Linux.
    if let Some((pid, rest)) = proc_magic(path, b"fd") {
        let num_str = rest.strip_prefix(b"/").unwrap_or(b"");
        let valid = !num_str.is_empty() && num_str.len() < 10
            && num_str.iter().all(|d| d.is_ascii_digit());
        if valid && sched::exists_probe(pid) == 0 {
            if sched::leaderless(pid) { return -2; }
            let tgt = sched::tgid_of(pid);
            if tgt != sched::current_tgid() {
                let c = sched::euid_of(current_pid());
                if c != 0 && c != sched::euid_of(tgt) { return -13; }
            }
            let mut fd = 0usize;
            for &d in num_str { fd = fd * 10 + (d - b'0') as usize; }
            let me = if tgt == sched::current_tgid() { current_pid() } else { tgt };
            // A socket names its inode as Linux does, `socket:[ino]`, with
            // the number fstat reports (the VFS cannot ask the net server).
            let h = vfs::resolve_fd(me, fd);
            if is_sock_handle(h) {
                let Some((_, ino, _)) = handle_stat(me, h) else { return -9 };
                let mut b = [0u8; 32];
                let mut n = 0;
                for &c in b"socket:[" { b[n] = c; n += 1; }
                let mut d = [0u8; 20];
                let mut k = 0;
                let mut v = ino;
                loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
                while k > 0 { k -= 1; b[n] = d[k]; n += 1; }
                b[n] = b']'; n += 1;
                return reply(&b[..n]);
            }
            let mut kb: Vec<u8> = alloc::vec![0u8; HOST_PATH_MAX];
            let msg = make_vfs_msg(vfs::VFS_FD_PATH, &[fd as u64, kb.as_mut_ptr() as u64, HOST_PATH_MAX as u64]);
            let n = vfs_reply_val(&vfs::handle(&msg, me));
            if n < 0 { return n; }
            let n = (n as usize).min(HOST_PATH_MAX);
            // Paths are made caller-relative; synthetic names ("pipe:[7]")
            // are returned as they are.
            return if n > 0 && kb[0] == b'/' { reply(&host_to_view(&kb[..n])) } else { reply(&kb[..n]) };
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

/// PATH_MAX: the longest path string a caller may pass, NUL included (a
/// 4096-byte string with no NUL is ENAMETOOLONG, as on Linux).
const KPATH_MAX: usize = vfs::PATH_MAX;
/// NAME_MAX: the longest single component.
const NAME_MAX: usize = vfs::NAME_MAX;
/// Longest host-absolute path handed to the VFS, NUL included: a PATH_MAX
/// path re-anchored under a chroot root or a deep cwd. See vfs::HOST_PATH_MAX.
const HOST_PATH_MAX: usize = vfs::HOST_PATH_MAX;

/// `AT_FDCWD` — "interpret a relative path against the process cwd".
const AT_FDCWD: usize = -100isize as usize;
/// `AT_EMPTY_PATH` — operate on `dirfd` itself when the path is "".
const AT_EMPTY_PATH: usize = 0x1000;
/// `AT_SYMLINK_NOFOLLOW` — act on a final symlink itself, not its target.
/// Shared by fstatat, fchmodat and fchownat; it used to be redeclared inside
/// each of them, which is how the latter two ended up silently ignoring it.
const AT_SYMLINK_NOFOLLOW: usize = 0x100;

/// A NUL-terminated, cwd-resolved absolute path held in kernel memory.
///
/// Handing the VFS a *kernel* pointer rather than the raw user pointer is
/// what makes cwd resolution possible at all. The VFS server and the mount
/// servers read path arguments by plain dereference of the pointer they are
/// given rather than through an address-space accessor. Kernel memory is
/// mapped in every address space, so such a pointer is valid everywhere the
/// old user pointer was — and it can carry a path we rewrote. The buffer is
/// owned by the calling syscall, and the caller stays blocked in `call_port`
/// for the whole round trip, so it outlives every server that reads it.
///
/// Heap-held: a path may be up to HOST_PATH_MAX bytes, and several of these
/// live in one syscall frame.
///
/// Two forms: `buf` is the *backing* path the VFS serves (alias mounts
/// applied, see vfs::alias), `ns` the *namespace* path when it differs —
/// what the cwd, the chroot floor and `..` operate on. They differ only under
/// an alias mount (a chroot's /proc, /dev, a bind mount).
struct KPath {
    /// Backing path + NUL.
    buf: Vec<u8>,
    /// Namespace path, when an alias rewrote it.
    ns: Option<Vec<u8>>,
}

impl KPath {
    /// Wrap a backing path verbatim (no alias translation).
    fn raw(bytes: &[u8]) -> KPath {
        let mut buf = Vec::with_capacity(bytes.len() + 1);
        buf.extend_from_slice(bytes);
        buf.push(0);
        KPath { buf, ns: None }
    }

    /// A namespace path (host-absolute, normalised), translated through the
    /// alias mounts and the `/proc/<pid>/{root,cwd}` magic links.
    fn from_ns(ns: Vec<u8>) -> Result<KPath, isize> {
        // Below /proc/self/fd/N: the namespace path is where that directory
        // really is, not the magic link, so chdir does not record a path
        // that depends on fd N staying open.
        if let Some(b) = proc_fd_below(vfs::alias::translate(&ns).as_deref().unwrap_or(&ns)) {
            if b.len() >= HOST_PATH_MAX { return Err(-36); }
            let root = sched::current_root_arc();
            let ns = vfs::alias::reverse(&b, root.as_deref()).unwrap_or_else(|| b.clone());
            let mut kp = KPath::raw(&b);
            kp.ns = Some(ns);
            return Ok(kp);
        }
        let mut denied = false;
        let walked = backing_walk_d(&ns, &mut false, &mut denied);
        if denied { return Err(-13); }
        let backing = match walked { Some(b) => b, None => {
            if ns.len() >= HOST_PATH_MAX { return Err(-36); }
            return Ok(KPath::raw(&ns));
        } };
        if backing.len() >= HOST_PATH_MAX || ns.len() >= HOST_PATH_MAX { return Err(-36); }
        let mut kp = KPath::raw(&backing);
        kp.ns = Some(ns);
        Ok(kp)
    }

    /// Pointer to pass to the VFS in place of the user `path_ptr`.
    #[inline]
    fn ptr(&self) -> usize { self.buf.as_ptr() as usize }
    /// The backing path (no NUL).
    #[inline]
    fn bytes(&self) -> &[u8] { &self.buf[..self.buf.len() - 1] }
    /// The namespace path: what chdir/chroot record.
    #[inline]
    fn ns_bytes(&self) -> &[u8] { self.ns.as_deref().unwrap_or(self.bytes()) }
}

/// `/proc/<pid|self|thread-self>/ns/<name>` → (pid, index into NS_NAMES).
fn proc_ns_link(p: &[u8]) -> Option<(u32, usize)> {
    let (pid, rest) = proc_magic(p, b"ns")?;
    let name = rest.strip_prefix(b"/")?;
    let k = sched::ns::NS_NAMES.iter().position(|n| *n == name)?;
    Some((pid, k))
}

/// `/proc/<pid|self|thread-self>/<what>` → (pid, rest after `<what>`).
/// ptrace-read-style check for exe/cwd/root of another process: same euid or root.
fn proc_link_allowed(pid: u32) -> bool {
    let tgt = sched::tgid_of(pid);
    if tgt == sched::current_tgid() { return true; }
    let c = sched::euid_of(current_pid());
    c == 0 || c == sched::euid_of(tgt)
}

fn proc_magic<'a>(p: &'a [u8], what: &[u8]) -> Option<(u32, &'a [u8])> {
    let rest = p.strip_prefix(b"/proc/")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (who, tail) = (&rest[..slash], &rest[slash + 1..]);
    let tail = tail.strip_prefix(what)?;
    if !(tail.is_empty() || tail[0] == b'/') { return None; }
    let pid = if who == b"self" || who == b"thread-self" {
        if who == b"self" { sched::current_tgid() } else { current_pid() }
    } else {
        if who.is_empty() || who.len() > 9 || !who.iter().all(|b| b.is_ascii_digit()) { return None; }
        let mut n = 0u32;
        for &d in who { n = n * 10 + (d - b'0') as u32; }
        // Translate through the caller's pid namespace: invisible pids are
        // ENOENT, and 1 names the namespace init, not the global init.
        sched::pidns::from_cur(n).ok()?
    };
    Some((pid, tail))
}

/// The backing path for namespace path `ns`, or `None` when it is the same:
/// alias mounts, then the `/proc/<pid>/root/...` and `/proc/<pid>/cwd/...`
/// magic links (only with something below them: the links themselves are
/// answered by readlink). Those resolve to that process's root / cwd, so a
/// jailed process's `/proc/self/root/x` stays in its jail.
fn backing_of(ns: &[u8]) -> Option<Vec<u8>> {
    backing_walk(ns, &mut false)
}

/// [`backing_of`], also reporting in `crossed` whether any step of the walk
/// went through an alias mount (all of which are nosuid).
fn backing_walk(ns: &[u8], crossed: &mut bool) -> Option<Vec<u8>> {
    backing_walk_d(ns, crossed, &mut false)
}

/// [`backing_walk`], also reporting in `denied` that a `/proc/<pid>/{root,cwd}`
/// hop named another user's process (the readlink gate, `proc_link_allowed`).
fn backing_walk_d(ns: &[u8], crossed: &mut bool, denied: &mut bool) -> Option<Vec<u8>> {
    *crossed |= vfs::alias::crosses(ns);
    let mut cur: Option<Vec<u8>> = vfs::alias::translate(ns);
    for _ in 0..4 {
        let p: &[u8] = cur.as_deref().unwrap_or(ns);
        if let Some(b) = proc_fd_below(p) { return Some(b); }
        let hit = proc_magic(p, b"root").map(|(pid, rest)| (pid, rest, true))
            .or_else(|| proc_magic(p, b"cwd").map(|(pid, rest)| (pid, rest, false)));
        let (pid, rest, is_root) = match hit { Some(h) if !h.1.is_empty() => h, _ => break };
        if sched::exists_probe(pid) == 0 && !proc_link_allowed(pid) { *denied = true; break; }
        let base: alloc::sync::Arc<[u8]> = if is_root {
            match sched::root_arc_of(pid) {
                Some(r) => r,
                None if sched::exists_probe(pid) == 0 => alloc::sync::Arc::from(&b"/"[..]),
                None => break,
            }
        } else {
            match sched::cwd_arc_of(pid) { Some(c) => c, None => break }
        };
        let mut v = Vec::with_capacity(base.len() + rest.len());
        if &*base != b"/" { v.extend_from_slice(&base); }
        v.extend_from_slice(rest);
        *crossed |= vfs::alias::crosses(&v);
        cur = Some(vfs::alias::translate(&v).unwrap_or(v));
    }
    cur
}

/// `/proc/<own pid>/fd/N/<rest>` → the backing path of the directory fd `N`
/// names, joined with `<rest>`, as Linux walks through the magic link. Only
/// with something below `N` (the link itself is answered by readlink and
/// reopened by open), and only the caller's own table.
///
/// The Claude Code CLI writes every file this way: it opens the parent with
/// O_PATH|O_DIRECTORY, readlinks /proc/self/fd/N to prove where it landed,
/// then mkdirs and creates through "/proc/self/fd/N/<name>" so a link swapped
/// in afterwards cannot redirect the write. With these paths ENOENT its file
/// writes failed ("ENOENT ... open") and its task-output pin was refused
/// ("task output swap refused (tasks dir moved or linked)"), so no shell
/// command could run.
///
/// The fd's path is its open-time path (the VFS has no dentries), so a
/// directory renamed after it was opened is not followed; a non-directory
/// fd ends in ENOTDIR from the filesystem, an fd with no path in ENOENT.
fn proc_fd_below(p: &[u8]) -> Option<Vec<u8>> {
    let (fd, below) = proc_own_fd(p)?;
    if below.is_empty() { return None; }
    let mut host = fd_host_path(fd)?;
    if host.last() != Some(&b'/') { host.push(b'/'); }
    host.extend_from_slice(below);
    Some(host)
}

/// `/proc/<own pid>/fd/N[/rest]` → (N, rest without its leading '/'), for
/// the caller's own descriptor table only (`self`, `thread-self` or a pid of
/// the calling thread group).
fn proc_own_fd(p: &[u8]) -> Option<(usize, &[u8])> {
    let (pid, rest) = proc_magic(p, b"fd")?;
    let rest = rest.strip_prefix(b"/")?;
    let (num, below) = match rest.iter().position(|&b| b == b'/') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &b""[..]),
    };
    if num.is_empty() || num.len() >= 10 || !num.iter().all(|d| d.is_ascii_digit()) { return None; }
    if sched::tgid_of(pid) != sched::current_tgid() { return None; }
    let mut fd = 0usize;
    for &d in num { fd = fd * 10 + (d - b'0') as usize; }
    Some((fd, below))
}

/// A host-absolute (backing) path as the calling task sees it: alias mounts
/// mapped back to where the task sees them, then the chroot prefix stripped
/// (getcwd's translation). Unjailed tasks, and paths not under the jail
/// root, come back unchanged.
fn host_to_view(p: &[u8]) -> Vec<u8> {
    let root = sched::current_root_arc();
    let ns = vfs::alias::reverse(p, root.as_deref());
    let p: &[u8] = ns.as_deref().unwrap_or(p);
    if let Some(r) = root.as_deref() {
        if r.len() > 1 && vfs::alias::under(p, r) {
            if p.len() == r.len() { return b"/".to_vec(); }
            return p[r.len()..].to_vec();
        }
    }
    p.to_vec()
}

/// Copy a NUL-terminated user string of at most `KPATH_MAX` bytes (NUL
/// included) into the kernel. EFAULT for an unreadable pointer, ENAMETOOLONG
/// when no NUL appears within `KPATH_MAX` bytes.
///
/// Copies page by page through the fault-tolerant uaccess copy, so a string
/// ending just before an unmapped page is fine and a bad pointer is an
/// errno, never a kernel fault.
fn copy_user_path(ptr: usize) -> Result<Vec<u8>, isize> {
    copy_user_str(ptr, KPATH_MAX)
}

/// [`copy_user_path`] with an explicit bound (`max` bytes, NUL included).
fn copy_user_str(ptr: usize, max: usize) -> Result<Vec<u8>, isize> {
    if ptr == 0 || !validate_user_buf(ptr, 1) { return Err(-14); }
    let mut out: Vec<u8> = Vec::new();
    if out.try_reserve(256).is_err() { return Err(-12); }
    let mut chunk = [0u8; 256];
    let mut p = ptr;
    loop {
        let page_left = 4096 - (p & 4095);
        let n = chunk.len().min(page_left).min(max - out.len());
        if n == 0 { return Err(-36); } // ENAMETOOLONG
        if unsafe { sched::uaccess::copy_from_user(chunk.as_mut_ptr(), p, n) } != 0 {
            return Err(-14);
        }
        if let Some(z) = chunk[..n].iter().position(|&b| b == 0) {
            out.extend_from_slice(&chunk[..z]);
            return Ok(out);
        }
        out.extend_from_slice(&chunk[..n]);
        p += n;
    }
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
/// `AT_EMPTY_PATH`, handled separately by `sys_newfstatat`), `-ENAMETOOLONG`
/// for a string of PATH_MAX bytes or more or a component over NAME_MAX.
fn resolve_user_path(path_ptr: usize) -> Result<KPath, isize> {
    let raw = copy_user_path(path_ptr)?;
    // An empty path is ENOENT, never "the cwd" — resolve_ns() would happily
    // hand back the cwd for it, silently turning stat("") into stat(".").
    if raw.is_empty() { return Err(-2); }
    KPath::from_ns(resolve_ns(&raw)?)
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
    let raw = copy_user_path(ptr)?;
    if raw.is_empty() { return Err(-2); } // ENOENT — empty target
    Ok(KPath::raw(&raw))
}

/// The absolute path an open descriptor was opened by, as the caller sees it
/// (chroot-relative, alias mounts mapped back). `None` when the fd names
/// nothing with a filesystem path (a pipe, an eventfd, a socket, a closed
/// fd, …).
///
/// This is the same `VFS_FD_PATH` query that answers
/// `readlink("/proc/self/fd/N")`: the VFS resolves RamFS and tmpfs entries
/// itself and forwards mounted files to the owning mount server, which
/// remembers the absolute path each `OpenFile` slot was opened by. Directory
/// descriptors are covered because a directory opens as an ordinary vnode on
/// both backends (a `TmpFile` slot with `is_dir`, or a `MountedFile`).
///
/// The VFS answers with the *host-absolute* path. For a chrooted caller that
/// path carries the jail-root prefix, and resolve_at_path re-anchors the base
/// at the jail root, so without stripping it here the base would carry the
/// prefix twice (a jailed openat(dirfd, "f") would look below
/// "<root><root>/…" and spuriously ENOENT).
fn fd_abs_path(fd: usize) -> Option<Vec<u8>> {
    let host = fd_host_path(fd)?;
    Some(host_to_view(&host))
}

/// The host-absolute (backing) path `fd` was opened by; see [`fd_abs_path`].
fn fd_host_path(fd: usize) -> Option<Vec<u8>> {
    // Descriptors the VFS does not own at all (sockets, epoll) would be
    // misinterpreted as VFS fd numbers — reject them up front.
    if fd >= net_server::SOCK_FD_BASE { return None; }
    let mut out: Vec<u8> = Vec::new();
    if out.try_reserve_exact(HOST_PATH_MAX).is_err() { return None; }
    out.resize(HOST_PATH_MAX, 0);
    let msg = make_vfs_msg(vfs::VFS_FD_PATH,
                           &[fd as u64, out.as_mut_ptr() as u64, HOST_PATH_MAX as u64]);
    let n = vfs_reply_val(&vfs::handle(&msg, current_pid()));
    if n <= 0 { return None; }
    let n = n as usize;
    // Only a real absolute path can serve as a resolution base; the synthetic
    // names ("pipe:[7]", "eventfd", …) must not.
    if n > HOST_PATH_MAX || out[0] != b'/' { return None; }
    out.truncate(n);
    Some(out)
}

/// Resolve an `*at()`-family path argument against its `dirfd`.
///
/// `AT_FDCWD` and absolute paths resolve against the process cwd exactly as
/// before. A relative path with a real `dirfd` is joined onto the directory
/// that descriptor names, which is what POSIX requires and what the
/// openat-relative traversal idiom (GNU fts, `uucore::safe_traversal`)
/// depends on.
///
/// When the descriptor has no path the VFS can name, this falls back to the
/// old cwd-relative behaviour rather than failing, so nothing that worked
/// before can start returning EBADF.
fn resolve_at_path(dirfd: usize, path_ptr: usize) -> Result<KPath, isize> {
    let raw = copy_user_path(path_ptr)?;
    if raw.is_empty() { return Err(-2); }
    KPath::from_ns(resolve_at_ns(dirfd, &raw)?)
}

/// The namespace path `raw` names relative to `dirfd` (see resolve_at_path).
fn resolve_at_ns(dirfd: usize, raw: &[u8]) -> Result<Vec<u8>, isize> {
    // dirfd is an `int`: only the low 32 bits count (a caller passing an int
    // through a variadic syscall() leaves the upper half undefined).
    let dirfd = dirfd as u32 as i32 as isize as usize;
    // An absolute path ignores the dirfd entirely, per POSIX.
    if dirfd == AT_FDCWD || raw.first() == Some(&b'/') { return resolve_ns(raw); }
    let base = match fd_abs_path(dirfd) {
        Some(b) => b,
        None    => return resolve_ns(raw),
    };
    // base + '/' + relative, then normalise "." / ".." the usual way.
    let mut joined = base;
    if joined.last() != Some(&b'/') { joined.push(b'/'); }
    joined.extend_from_slice(raw);
    resolve_ns(&joined)
}

/// Resolve a path to an absolute, normalised *namespace* path (host-absolute:
/// the chroot root prefixed), handling ".." and "." lexically.
///
/// An absolute path starts at the caller's root, a relative one at its cwd.
/// `..` never climbs above the chroot root. ENAMETOOLONG for a component
/// longer than NAME_MAX or a result of HOST_PATH_MAX bytes or more.
fn resolve_ns(path: &[u8]) -> Result<Vec<u8>, isize> {
    // `path` may carry a NUL terminator; only process up to it.
    let path = match path.iter().position(|&b| b == 0) { Some(n) => &path[..n], None => path };

    // The caller's chroot root (host-absolute). `None` means "not chrooted" —
    // the overwhelmingly common case, which takes the same path it always did.
    let root = sched::current_root_arc();
    let rlen = root.as_ref().map(|r| r.len()).filter(|&n| n > 1).unwrap_or(0);
    // `..` may never climb above this offset. For a jailed task it is the end
    // of the root prefix; otherwise it is 1 (the real "/").
    let floor = if rlen > 1 { rlen } else { 1 };

    let mut out: Vec<u8> = Vec::new();
    if out.try_reserve(path.len() + rlen + 64).is_err() { return Err(-12); }
    if path.first() == Some(&b'/') {
        // An absolute path is relative to the jail root, so start there.
        match &root { Some(r) if rlen > 1 => out.extend_from_slice(r), _ => out.push(b'/') }
    } else {
        match sched::current_cwd_arc() {
            Some(c) if c.first() == Some(&b'/') => out.extend_from_slice(&c),
            _ => out.push(b'/'),
        }
        // A cwd that somehow lies outside the jail (it cannot be set there)
        // must not let `..` walk the rest of the host: clamp the floor.
        if rlen > 1 && !vfs::alias::under(&out, root.as_deref().unwrap_or(b"/")) {
            out.clear();
            out.extend_from_slice(root.as_deref().unwrap());
        }
    }

    for comp in path.split(|&b| b == b'/') {
        if comp.is_empty() || comp == b"." { continue; }
        if comp == b".." {
            if out.len() > floor {
                let mut last = out.len() - 1;
                while last > 0 && out[last] != b'/' { last -= 1; }
                // Clamp at the jail root: `..` from the top of the jail stays
                // put, which is what makes the chroot a boundary rather than
                // a suggestion.
                let n = if last < floor { floor } else if last == 0 { 1 } else { last };
                out.truncate(n);
            }
            continue;
        }
        if comp.len() > NAME_MAX { return Err(-36); }
        if out.last() != Some(&b'/') { out.push(b'/'); }
        out.extend_from_slice(comp);
        if out.len() >= HOST_PATH_MAX { return Err(-36); }
    }
    if out.is_empty() { out.push(b'/'); }
    if out.len() > 1 && out.last() == Some(&b'/') { out.pop(); }
    // The void root's name is not observable from outside it.
    if vfs::alias::under(&out, vfs::VOID_ROOT) && root.as_deref() != Some(vfs::VOID_ROOT) {
        return Err(-2);
    }
    Ok(out)
}

/// Read a user cstr of at most 256 bytes (NUL included) — the mount(2)
/// fstype, which is not a path. EFAULT/ENAMETOOLONG as copy_user_str.
fn read_user_short_str(ptr: usize) -> Result<Vec<u8>, isize> {
    copy_user_str(ptr, 256)
}

/// Teach the scheduler how to release a dying task's fds.
///
/// `sched::exit` is the single funnel every death path goes through — the
/// `EXIT`/`EXIT_GROUP` syscalls, but also `SIG_DFL`-terminate signal delivery
/// and the two signal-frame failure paths in `sched/src/signal.rs`, which
/// called `exit()` directly and skipped fd teardown entirely. A process killed
/// by a signal while holding a pipe write end therefore left the ring's writer
/// count non-zero forever, wedging the reader and leaking one of only 16 pipe
/// slots. Registering the teardown with the scheduler closes that gap for
/// every present and future caller of `exit()` at once, rather than patching
/// each signal site.
///
/// `sched` cannot call this directly: it sits below the kernel crate and has
/// no knowledge of the VFS, which is why this goes through a function pointer.
pub fn init_exit_teardown() {
    sched::register_exit_teardown_hook(vfs_close_all_for);
    sched::register_session_leader_exit_hook(tty_server::pty::session_leader_exited);
}

/// Close all FDs, sockets, TTY handles and epoll instances owned by `pid`.
///
/// The exit teardown hook (see `init_exit_teardown`), so `exit_group`'s forced-kill loop
/// (`EXIT_GROUP` below) can release a *sibling* thread's resources too — that
/// thread never gets to run its own `EXIT` syscall, since it's being killed
/// out from under it.
/// A fork/vfork whose `before_enqueue` hook already duplicated the parent's fd
/// and socket tables for `child`, and which then failed (the task table was
/// full at enqueue): release everything the hook took, or those tables stay
/// claimed by a pid that never ran. `child == 0`: the hook never ran.
fn undo_unborn_child(child: u32) {
    if child == 0 { return; }
    vfs_close_all_for(child);
    sched::clear_exe_path(child);
}

fn vfs_close_all_for(pid: u32) {
    // A thread's private fd table dies with its last user.
    if let Some((key, _tgid, last)) = sched::fdkey::drop(pid) {
        if last {
            for (_, h) in vfs::handle_fds(key) { close_handle(pid, h); }
            let m = make_vfs_msg(vfs::VFS_CLOSE_ALL, &[key as u64]);
            let _ = vfs::handle(&m, pid);
        }
    }
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
    // A VT held in VT_PROCESS by a process that just died would otherwise wait
    // for a release signal nobody will answer, and only the 5 s watchdog would
    // get the display back.
    tty_server::vt::cleanup_pid(pid);
    // An EVIOCGRAB held by a process that just died would silence the node for
    // everyone, and a grabbed node produces no events for `broadcast` to
    // self-heal on. See TODO.md item 20.
    evdev_server::cleanup_pid(pid);
    stdio_flags_close_all(pid);
}

/// sys_ioctl — try VFS first (FIONREAD on pipes/files), then TTY server.
fn sys_ioctl(fd: usize, cmd: usize, arg: usize, frame_ptr: usize) -> isize {
    let pid = current_pid();
    // musl's `int ioctl(int fd, int request, ...)` sign-extends the request when
    // it forwards to the raw syscall, so a request with bit 31 set (every _IOWR
    // ioctl, e.g. DRM_IOCTL_PRIME_HANDLE_TO_FD = 0xC00C642D) arrives here as
    // 0xFFFFFFFF_C00C642D. Exact-match dispatch below (PRIME intercept, the DRM
    // 0x100x constants) would miss it and fall through to the DRM server, which
    // returns EPERM for PRIME — the whole reason anvil's gbm_bo_get_fd failed
    // while drmsmoke (Rust libc, c_ulong request, zero-extended) succeeded. Mask
    // to the 32-bit ioctl request the kernel actually means.
    let cmd = cmd & 0xFFFF_FFFF;
    // Per-request census rows in /proc/sysstat (`sched::sysacct::NR_SUB_BASE`).
    sched::note_syscall_subkey(if (cmd >> 8) & 0xff == 0x64 { 0x800 | (cmd as u32 & 0xff) }
        else if cmd & 0x7ff != 0 { cmd as u32 & 0x7ff } else { 0x7ff });
    const FIONREAD: usize = 0x541B;
    const FBIOGET_VSCREENINFO: usize = 0x4600;
    const ENOTTY: isize = -25;

    // Console proxies (/dev/tty, dup'd stdio fds) answer terminal ioctls
    // exactly like the console fd they alias — crossterm probes TIOCGWINSZ
    // and termios on its /dev/tty handle.
    // A VT node is console_stdio for read/write purposes, but folding one to 0
    // here would erase the very thing the VT dispatch below needs: /dev/tty2's
    // ioctls would arrive addressed to "the active VT". `fd_vt_number` is only
    // consulted for fds that already answered true, i.e. never on the DRM or
    // evdev ioctl paths.
    // The fold only picks the console's ioctl routing: a proxy IS the
    // console, so the fd<=2 "redirected stdio is not a tty" guard below must
    // not then judge it by what fd 0 happens to be now (with stdin a pipe, a
    // /dev/tty fd answered ENOTTY to isatty -- lane procfix).
    let console_proxy = fd > 2 && fd < net_server::SOCK_FD_BASE
        && vfs::fd_is_console_stdio(pid, fd)
        && vfs::fd_vt_number(pid, fd).is_none();
    let fd = if console_proxy { 0 } else { fd };

    if cmd == FIONREAD && fd == 0 {
        if arg == 0 || !validate_user_buf(arg, 4) { return -14; }
        let has_data = crate::serial_has_data();
        return if sched::uaccess::write_user(arg, if has_data { 1i32 } else { 0 }) { 0 } else { -14 };
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
        let nonblocking = match sched::uaccess::read_user::<i32>(arg) { Some(v) => v != 0, None => return -14 };
        let get_msg = make_vfs_msg(vfs::VFS_FCNTL, &[fd as u64, F_GETFL as u64, 0]);
        let cur = vfs_reply_val(&vfs::handle(&get_msg, pid));
        if cur < 0 { return cur; }
        let flags = if nonblocking { cur as usize | O_NONBLOCK } else { cur as usize & !O_NONBLOCK };
        let set_msg = make_vfs_msg(vfs::VFS_FCNTL, &[fd as u64, F_SETFL as u64, flags as u64]);
        return vfs_reply_val(&vfs::handle(&set_msg, pid));
    }
    // FIONBIO on a socket fd — Rust std's UnixListener/UnixStream::set_nonblocking
    // issues ioctl(FIONBIO) (not fcntl F_SETFL). Route it through the net server's
    // NET_GETFL/NET_SETFL so it toggles the very same O_NONBLOCK bit that fcntl
    // F_SETFL and SOCK_NONBLOCK-at-creation use — one flag, not a second. Without
    // this the wayland-server rs backend's listening socket died at bind time with
    // ENOTTY. The user int is read (validate + raw read) before any net_server
    // lock is taken, honoring the no-user-memory-under-spinlock invariant.
    if cmd == FIONBIO && fd >= net_server::SOCK_FD_BASE && fd < EPOLL_FD_BASE {
        if arg == 0 || !validate_user_buf(arg, 4) { return -14; }
        const O_NONBLOCK: usize = 0x800;
        let nonblocking = match sched::uaccess::read_user::<i32>(arg) { Some(v) => v != 0, None => return -14 };
        let get_msg = make_vfs_msg(net_server::NET_GETFL, &[fd as u64]);
        let cur = net_reply_val(&net_server::handle(&get_msg, pid));
        if cur < 0 { return cur; }
        let flags = if nonblocking { cur as usize | O_NONBLOCK } else { cur as usize & !O_NONBLOCK };
        let set_msg = make_vfs_msg(net_server::NET_SETFL, &[fd as u64, flags as u64]);
        return net_reply_val(&net_server::handle(&set_msg, pid));
    }
    // FIONREAD / TIOCOUTQ (SIOCINQ / SIOCOUTQ) on a socket fd: answered by the
    // net server from the connection's own queues. They used to fall through
    // to the VFS below, which does not know socket fds and said EBADF — see
    // net_server::handle_queue_len for what that broke.
    const TIOCOUTQ: usize = 0x5411;
    if (cmd == FIONREAD || cmd == TIOCOUTQ)
        && fd >= net_server::SOCK_FD_BASE && fd < EPOLL_FD_BASE
    {
        if arg == 0 || !validate_user_buf(arg, 4) { return -14; }
        let msg = make_vfs_msg(net_server::NET_QUEUE_LEN, &[fd as u64, (cmd == TIOCOUTQ) as u64]);
        let n = net_reply_val(&net_server::handle(&msg, pid));
        if n < 0 { return n; }
        // Written with no net-server lock held (the handler has returned).
        return if sched::uaccess::write_user(arg, n.min(i32::MAX as isize) as i32) { 0 } else { -14 };
    }

    // SIOCGIF* (getifaddrs' ioctl fallback, if_nameindex, if_indextoname,
    // nICEr's SIOCGIFFLAGS): answered from the net server's interface table
    // (`net_server::rtnetlink`). The user buffers are copied with no lock held.
    if fd >= net_server::SOCK_FD_BASE && fd < EPOLL_FD_BASE
        && net_server::rtnetlink::is_if_ioctl(cmd as u32)
    {
        use net_server::rtnetlink as nl;
        let links = nl::current_links();
        if cmd as u32 == nl::SIOCGIFCONF {
            // struct ifconf { int ifc_len; (pad) char *ifc_buf; }
            if arg == 0 { return -14; }
            let (len, buf) = match (sched::uaccess::read_user::<i32>(arg), sched::uaccess::read_user::<usize>(arg + 8)) {
                (Some(l), Some(b)) => (l.max(0) as usize, b), _ => return -14,
            };
            let all = nl::ifconf(&links, usize::MAX);
            let out = if buf == 0 { &all[..] } else { &all[..all.len().min(len) / nl::IFREQ_LEN * nl::IFREQ_LEN] };
            if buf != 0 && !out.is_empty() {
                if !validate_user_buf(buf, out.len()) { return -14; }
                if unsafe { sched::uaccess::copy_raw(buf as *mut u8, out.as_ptr(), out.len()) } != 0 { return -14; }
            }
            return if sched::uaccess::write_user(arg, out.len() as i32) { 0 } else { -14 };
        }
        if arg == 0 || !validate_user_buf(arg, nl::IFREQ_LEN) { return -14; }
        let mut ifr = match sched::uaccess::read_user::<[u8; nl::IFREQ_LEN]>(arg) { Some(v) => v, None => return -14 };
        let r = nl::ifreq_ioctl(cmd as u32, &mut ifr, &links);
        if r < 0 { return r as isize; }
        return if sched::uaccess::write_user(arg, ifr) { 0 } else { -14 };
    }

    // ── Block devices ────────────────────────────────────────────────────────
    // BLKGETSIZE64/BLKSSZGET/BLKPG/LOOP_* live in the VFS's block registry,
    // which owns the node, its size and its partition table. Dispatched on the
    // *fd kind* rather than on the command number because the LOOP_* range
    // (0x4C00..) collides with nothing here but the BLK* range (0x12xx) would
    // otherwise have to be enumerated twice — and because an ioctl aimed at a
    // block device must never fall through to the TTY server's ENOTTY.
    if let Some(vfs::VnodeKind::BlockDev { .. }) = vfs::vfs_get_node_kind(pid, fd) {
        let msg = make_vfs_msg(vfs::VFS_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
        return vfs_reply_val(&vfs::handle(&msg, pid));
    }

    // DRM ioctl commands
    const DRM_IOCTL_GET_MODE: usize = 0x1003;
    const DRM_IOCTL_SET_MODE: usize = 0x1001;
    const DRM_IOCTL_CREATE_FB: usize = 0x1002;
    const DRM_IOCTL_FLIP_PAGE: usize = 0x1004;
    const DRM_IOCTL_SET_PLANE: usize = 0x1005;
    const DRM_IOCTL_GET_CAPS: usize = 0x1006;
    // Read-only virtio-gpu completion-interrupt census (drmsmoke's regression
    // test of the interrupt path); eight u64s written back.
    const DRM_IOCTL_GPU_IRQ_STATS: usize = 0x1008;
    // Read/set the control queue's spin-before-park interval (root; one u64,
    // new value in, old value out). drmsmoke uses it to force a parked wait.
    const DRM_IOCTL_GPU_PARK_SPIN: usize = 0x1009;
    if cmd == DRM_IOCTL_GPU_PARK_SPIN && (arg == 0 || !validate_user_buf(arg, 8)) { return -14; } // EFAULT
    const DRM_IOCTL_CTX_DRAIN_US: usize = 0x100A;
    if cmd == DRM_IOCTL_CTX_DRAIN_US && (arg == 0 || !validate_user_buf(arg, 8)) { return -14; } // EFAULT

    // Check if it's a standard Linux EVDEV (type 'E' = 0x45) or DRM (type 'd' = 0x64) ioctl
    let ioctl_type = (cmd >> 8) & 0xFF;
    let is_evdev = ioctl_type == 0x45;
    let is_drm = ioctl_type == 0x64;

    // ── i915 legacy GEM_MMAP (lane i915ddb) ──────────────────────────────────
    // Linux i915_gem_mmap_ioctl returns a CPU address: vm_mmap of the object
    // (MAP_SHARED, RW) at `offset`, write-combined with I915_MMAP_WC. The DRM
    // server cannot map into the caller, so the ioctl is answered here, in the
    // caller's address space: GEM_MMAP_OFFSET (WC, or WB without the flag) on
    // the same fd, then mmap() of that opaque offset -- the same path (caching,
    // ownership check, counters) Mesa's MMAP_OFFSET users take.
    //     struct drm_i915_gem_mmap (40 bytes): handle u32 @0 | pad u32 @4 |
    //     offset u64 @8 | size u64 @16 | addr_ptr u64 @24 | flags u64 @32
    // MMAP_OFFSET's 32-byte struct (handle, pad, offset, flags, extensions) is
    // built in place in the caller's buffer; the fields are restored after.
    const DRM_IOCTL_I915_GEM_MMAP: usize = 0xC028645E;
    const DRM_IOCTL_I915_GEM_MMAP_OFFSET: usize = 0xC0206464;
    if cmd == DRM_IOCTL_I915_GEM_MMAP {
        use sched::uaccess::{read_user, write_user};
        if arg == 0 || !validate_user_buf(arg, 40) { return -14; } // EFAULT
        let mut f = [0u64; 5];
        for (i, v) in f.iter_mut().enumerate() {
            match read_user::<u64>(arg + 8 * i) { Some(x) => *v = x, None => return -14 }
        }
        let (off, size, flags) = (f[1], f[2], f[4]);
        const I915_MMAP_WC: u64 = 1;
        if flags & !I915_MMAP_WC != 0 { return -22; } // EINVAL, as Linux
        if size == 0 || off & 0xFFF != 0 || off.checked_add(size).is_none() { return -22; }
        let mo_flags: u64 = if flags & I915_MMAP_WC != 0 { 1 } else { 2 }; // MMAP_OFFSET_WC / _WB
        let tmp = [f[0] & 0xFFFF_FFFF, 0, mo_flags, 0];
        for (i, v) in tmp.iter().enumerate() { if !write_user(arg + 8 * i, *v) { return -14; } }
        let r = sys_ioctl(fd, DRM_IOCTL_I915_GEM_MMAP_OFFSET, arg, frame_ptr);
        let fake = read_user::<u64>(arg + 8).unwrap_or(0);
        for (i, v) in f.iter().enumerate().take(4) { let _ = write_user(arg + 8 * i, *v); }
        if r < 0 { return r; }
        // range_overflows(offset, size, obj size) -> EINVAL: the last byte must
        // resolve inside the same object, contiguous with its first.
        let last = fake.wrapping_add(off).wrapping_add(size - 1);
        match (drivers::drm_offset::resolve(fake), drivers::drm_offset::resolve(last)) {
            (Some(a), Some(b)) if b == a + off + size - 1 => {}
            _ => return -22,
        }
        const PROT_RW: usize = 3;
        const MAP_SHARED: usize = 1;
        // The device mmap names whole objects (its ownership check is on the
        // object's token), so map from the object's start and drop the head.
        let a = sys_mmap(0, (off + size) as usize, PROT_RW, MAP_SHARED, fd, fake as usize);
        if a < 0 { return a; }
        if off != 0 { let _ = sys_unmap_mem(a as usize, off as usize); }
        return if write_user(arg + 24, a as u64 + off) { 0 } else { -14 };
    }

    // Sync files (syncobj <-> sync_file, dma-buf implicit fences, SYNC_IOC_*):
    // they mint or consume fds, so they are answered here. `None` unless the
    // gate in drivers::sync_file is open and the fd/flags are ones it handles.
    if let Some(r) = syncfile_ioctl::intercept(pid, fd, cmd, arg) { return r; }
    if let Some(r) = v4l2::intercept(pid, fd, cmd, arg) { return r; }

    // ── PRIME / dmabuf export+import (intercepted here, not in the DRM server) ──
    // The DRM server (drivers crate) has no fd-table access; building a dmabuf fd
    // is a VFS op. This syscall layer runs in the caller's AS with pid + both the
    // `drivers` and `vfs` crates in scope, so it is the one place that can turn a
    // GEM handle into a real fd. Both BO kinds resolve here: a dumb buffer or a
    // guest-backed blob exports an fd whose frames ARE its pages, while a pure
    // host-side Venus blob exports a token fd with no pages (see
    // `prime_export_acquire` and `install_dmabuf_vmo`).
    // struct drm_prime_handle { u32 handle@0; u32 flags@4; s32 fd@8; } = 12 bytes.
    const DRM_IOCTL_PRIME_HANDLE_TO_FD: usize = 0xC00C642D;
    const DRM_IOCTL_PRIME_FD_TO_HANDLE: usize = 0xC00C642E;
    if cmd == DRM_IOCTL_PRIME_HANDLE_TO_FD {
        if arg == 0 || !validate_user_buf(arg, 12) { return -14; } // EFAULT
        let handle = match sched::uaccess::read_user::<u32>(arg) { Some(v) => v, None => return -14 };
        // `drm_prime_handle.flags`: DRM_CLOEXEC (0x2) asks for an fd that does
        // not survive exec, and every Mesa/libdrm export passes it. Ignoring it
        // handed the compositor's swapchain dmabufs to its kiosk child — the
        // greeter inherited a reference on every scanout buffer across
        // fork+exec, and a greeter that outlived its compositor pinned them.
        const DRM_CLOEXEC: u32 = 0x2;
        let prime_flags = match sched::uaccess::read_user::<u32>(arg + 4) { Some(v) => v, None => return -14 };
        let open_cloexec: u64 = if prime_flags & DRM_CLOEXEC != 0 { 0x8_0000 } else { 0 };
        // Scope the lookup to the calling open, exactly as every other
        // handle-consuming virtgpu ioctl does (b80ab5a). A card/render fd
        // carries a per-open identity; anything else resolves to 0, which
        // `blob_lookup` treats as "no identity to check in either direction" —
        // the same rule the legacy Driver::handle path already relies on. Dumb
        // buffers are unscoped either way, by the note on `open_may_reach`.
        let open_id = match vfs::vfs_get_node_kind(pid, fd) {
            Some(vfs::VnodeKind::DynamicDevice { open_id, .. }) => open_id,
            _ => 0,
        };
        // Takes ONE reference on the BO object, which this syscall now owns and
        // must dispose of on every path: `install_dmabuf_vmo` accepts it on
        // success, `bo_release_exported` takes it back on failure. That
        // reference is what makes the exported fd keep the buffer alive across
        // GEM_CLOSE — without it, `read(fd)` after a close walked frames the
        // buddy allocator had already handed to someone else.
        let backing = match drivers::drm_device_interface::prime_export_acquire(handle, open_id) {
            Some(v) => v,
            None => return -22, // EINVAL: no BO this open may reach
        };
        // Linux keeps ONE dma-buf per GEM object: a re-export while one is
        // alive is a new fd on that same file. Do the same (i915 objects for
        // now) — every fresh export below costs a slot of the global tmpfs
        // pool, which the laptop's Firefox/YouTube session ran dry. The
        // existing slot already holds its own reference on the object, so
        // the one just acquired goes straight back.
        if drivers::i915::prime_dedup(backing.obj) {
            if let Some(nfd) = vfs::dmabuf_share_existing(pid, backing.obj, open_cloexec != 0) {
                drivers::drm_device_interface::bo_release_exported(backing.obj);
                return if sched::uaccess::write_user(arg + 8, nfd as i32) { 0 } else { -14 };
            }
        }
        // Open an ephemeral tmpfs node (same path memfd_create uses) to get a real
        // fd + owner slot, then promote that slot to a borrowed dmabuf VMO whose
        // frames ARE the dumb buffer's pages.
        static DMABUF_SEQ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        let n = DMABUF_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let mut path = [0u8; 32];
        let prefix = b"/tmp/dmabuf:";
        path[..prefix.len()].copy_from_slice(prefix);
        let mut plen = prefix.len();
        // append decimal n
        let mut digits = [0u8; 10]; let mut dn = 0usize; let mut v = n;
        loop { digits[dn] = b'0' + (v % 10) as u8; dn += 1; v /= 10; if v == 0 { break; } }
        while dn > 0 { dn -= 1; path[plen] = digits[dn]; plen += 1; }
        let open_msg = make_vfs_msg(vfs::VFS_OPEN, &[
            path.as_ptr() as u64,
            (0x042 | 0x200) as u64 | open_cloexec, // O_RDWR|O_CREAT|O_TRUNC [|O_CLOEXEC]
            0o600u64,
        ]);
        let newfd = vfs_reply_val(&vfs::handle(&open_msg, pid));
        if newfd < 0 {
            // The BO reference is ours and there is now nothing to give it to.
            drivers::drm_device_interface::bo_release_exported(backing.obj);
            return newfd;
        }
        // The tmpfs fd table is keyed by TGID: vfs::handle canonicalises caller_pid
        // via sched::tgid_of, so the fd just opened lives in the *process* table,
        // not this thread's. install_dmabuf_vmo/dmabuf_handle_of index the table
        // directly (find_tbl, no remap), so they MUST be handed the same TGID — a
        // compositor issues PRIME from a render thread whose TID != TGID (this is
        // exactly why cosmic-comp's first few exports, on the main thread, worked
        // and every later one on the render thread failed to install). Unlinking
        // the name still goes through vfs::handle, so it takes the raw pid.
        let vpid = sched::tgid_of(pid);
        let n_pages = if backing.pages != 0 { backing.pages } else { 1usize << backing.order };
        // L1.10: a scattered virgl buffer (phys 0) exports its frame list.
        let sg_pages = if backing.phys == 0 {
            drivers::drm_device_interface::sg_pages_of_obj(backing.obj)
        } else { None };
        let installed = match sg_pages {
            Some(pages) => vfs::install_dmabuf_vmo_pages(vpid, newfd as usize, pages,
                                                          backing.len, backing.obj, handle),
            None => vfs::install_dmabuf_vmo(vpid, newfd as usize, backing.phys, n_pages,
                                            backing.len, backing.obj, handle),
        };
        if !installed {
            // Never leak the ephemeral node/fd on the failure path: close the fd
            // (drops the pool slot) and unlink the name. Without this a failed
            // export would pin a /tmp slot + one of the 64 process fds forever,
            // and a per-frame compositor would exhaust both within a second.
            let close_msg = make_vfs_msg(vfs::VFS_CLOSE, &[newfd as u64]);
            let _ = vfs::handle(&close_msg, pid);
            let unlink_msg = make_vfs_msg(vfs::VFS_UNLINK, &[path.as_ptr() as u64]);
            let _ = vfs::handle(&unlink_msg, pid);
            // The slot never took the reference (that is what `false` means),
            // so it is still ours. Released AFTER the close/unlink, not before,
            // so the two dispositions cannot interleave: on this path the slot
            // holds no `dmabuf_obj`, the close releases nothing, and this is
            // the one and only drop.
            drivers::drm_device_interface::bo_release_exported(backing.obj);
            return -12; // ENOMEM / install failed
        }
        // Unlink the /tmp/dmabuf:<n> name immediately: the open fd holds the slot
        // alive (create-then-unlink tempfile idiom, see servers/vfs tmp_nlink), so
        // it behaves as an anonymous memfd. Without this, every exported frame
        // leaks a named node whose borrowed-VMO length is charged to the tmpfs
        // quota — a buffer-churning compositor (cosmic-comp reallocates its
        // scanout buffer per frame, unlike a fixed swapchain) exhausts /tmp after
        // a few dozen exports (ENOSPC), so gbm_bo_get_fd_for_plane then fails and
        // nothing is ever presented. Reclaimed when the client closes the fd.
        let unlink_msg = make_vfs_msg(vfs::VFS_UNLINK, &[path.as_ptr() as u64]);
        let _ = vfs::handle(&unlink_msg, pid);
        // Write the fd back into drm_prime_handle.fd (offset 8).
        return if sched::uaccess::write_user(arg + 8, newfd as i32) { 0 } else { -14 };
    }
    if cmd == DRM_IOCTL_PRIME_FD_TO_HANDLE {
        if arg == 0 || !validate_user_buf(arg, 12) { return -14; } // EFAULT
        let dfd = match sched::uaccess::read_user::<i32>(arg + 8) { Some(v) => v, None => return -14 };
        // Same TGID canonicalisation as HANDLE_TO_FD: the dmabuf fd lives in the
        // process (TGID) table, but dmabuf_handle_of indexes it directly.
        let tgid = sched::tgid_of(pid);
        let exporter_handle = match vfs::dmabuf_handle_of(tgid, dfd as usize) {
            Some(h) => h,
            None => return -22, // EINVAL
        };
        // A blob is reachable only by the open that owns a handle on it, so the
        // importer needs a handle of its own (`prime_import_blob`); echoing the
        // exporter's handle is right only for a dumb buffer, whose handles are
        // global. The import is scoped to the calling open exactly as the
        // export above is.
        let open_id = match vfs::vfs_get_node_kind(pid, fd) {
            Some(vfs::VnodeKind::DynamicDevice { open_id, .. }) => open_id,
            _ => 0,
        };
        // A dumb / virgl 3D object gets a per-open alias the same way
        // (`prime_import_dumb`): echoing the exporter's number let the
        // importer's GEM_CLOSE retire the exporter's handle.
        let obj = vfs::dmabuf_obj_of(tgid, dfd as usize);
        let handle = obj
            .and_then(|obj| drivers::drm_device_interface::prime_import_blob(obj, open_id))
            .or_else(|| obj.and_then(|obj| drivers::drm_device_interface::prime_import_dumb(obj, open_id)))
            .unwrap_or(exporter_handle);
        if FD_PROVENANCE_BT { prime_import_note(open_id, handle, user_backtrace(frame_ptr)); }
        drivers::i915::ptecheck_fd_to_handle(dfd, open_id, obj, handle);
        return if sched::uaccess::write_user(arg, handle) { 0 } else { -14 };
    }

    // ── VIRTGPU_EXECBUFFER's out-fence fd (intercepted here for the same reason
    //    PRIME is: the DRM server has no fd table) ──────────────────────────────
    //
    // `struct drm_virtgpu_execbuffer` (64 bytes, which is what the 0x0040 size
    // field of the request code encodes):
    //     flags u32 @0 | size u32 @4 | command u64 @8 | bo_handles u64 @16
    //     num_bo_handles u32 @24 | fence_fd s32 @28 | ring_idx u32 @32 | …
    //
    // WHY THIS EXISTS. Mesa's venus backend compiles SIMULATE_SYNCOBJ/
    // SIMULATE_SUBMIT unconditionally (Mesa 25.3.6, vn_renderer_virtgpu.c). Two
    // things follow, and they must land together:
    //
    //   1. `sim_syncobj_create` probes once per process with a zero-size, zero-
    //      command execbuffer carrying FENCE_FD_OUT, and requires both `ret == 0`
    //      and `fence_fd >= 0`. Failing it disables every `vn_renderer_sync`,
    //      which is how `vn_ring_destroy`'s ring-teardown submit gets skipped.
    //   2. `sim_submit` sets FENCE_FD_OUT whenever `batch->sync_count != 0` and
    //      then unconditionally `close(args.fence_fd)`. `args` is a designated
    //      initialiser, so an unwritten `fence_fd` is **0** — i.e. `close(0)`,
    //      closing the process's stdin. Path (2) is reachable only once (1)
    //      succeeds, so accepting the probe without also writing `fence_fd` would
    //      ARM that, on every vkDestroyInstance. Hence: one change, and the write
    //      lives here, above the accept, where nothing can bypass it.
    //
    // WHY AN EVENTFD, AND WHEN IT SIGNALS. Mesa's entire use of this fd is
    // `poll(POLLIN)` (`sim_syncobj_poll`), `fcntl(F_DUPFD_CLOEXEC)` and `close`.
    // It never `read`s it, so an eventfd whose counter goes 0 → 1 once is a
    // fence: unreadable until the work retires, readable forever after.
    // Submission is asynchronous (`VirtioGpu::submit_async` returns at the
    // kick), so the fd is minted at ZERO and armed on the submission's fence
    // through `drm_device_interface::out_fence_register`; the DRM tick adds one
    // to it when the host retires that fence. It used to be minted pre-signalled,
    // which was true only while `submit` busy-spun for the reply — the
    // dependency TODO.md's FENCE_FD_OUT note warned about, now paid.
    //
    // ORDERING follows upstream `virtio_gpu_execbuffer_ioctl`: reserve the out
    // fence BEFORE submitting, so a submission is never charged for an fd-table
    // failure, and release it if the submission itself fails.
    //
    // INVARIANT, stated because the whole defect is the negation of it: whenever
    // the caller set FENCE_FD_OUT, `fence_fd` is written before we return —
    // with a real fd on success, and with **-1** on every failure path. It is
    // never left holding the caller's incoming value.
    const DRM_IOCTL_VIRTGPU_EXECBUFFER: usize = 0xC0406442;
    if cmd == DRM_IOCTL_VIRTGPU_EXECBUFFER {
        const EXECBUF_FENCE_FD_OUT: u32 = 0x02;
        const FENCE_FD_OFF: usize = 28;
        const EFD_CLOEXEC: u64 = 0x8_0000;
        if arg == 0 || !validate_user_buf(arg, 64) { return -14; } // EFAULT
        let flags = match sched::uaccess::read_user::<u32>(arg) { Some(v) => v, None => return -14 };
        // FENCE_FD_IN (lane gpuq): upstream `sync_file_get_fence(fence_fd)`
        // then an interruptible CPU wait before queueing. The fd table lives
        // here, so the in-fence is resolved and waited on here, before the
        // driver sees the submission. Not a fence fd -> EINVAL, as upstream.
        const EXECBUF_FENCE_FD_IN: u32 = 0x01;
        if flags & EXECBUF_FENCE_FD_IN != 0 {
            let in_fd = match sched::uaccess::read_user::<i32>(arg + FENCE_FD_OFF) { Some(v) => v, None => return -14 };
            use drivers::syncobj_core::Fence as SFence;
            let fence = if in_fd < 0 { None } else {
                match vfs::eventfd_fence_meta(pid, in_fd as usize) {
                    Some((vfs::EVFD_ORIGIN_SYNC_FILE, tok)) => Some(drivers::sync_file::fence_of(tok)),
                    Some((vfs::EVFD_ORIGIN_FENCE, 0)) => Some(SFence::Signaled),
                    Some((vfs::EVFD_ORIGIN_FENCE, f)) => Some(SFence::One(f)),
                    _ => None,
                }
            };
            let rc = match fence {
                None => -22,
                Some(f) => match {
                    drivers::drm_device_interface::EXEC_IN_FENCE_WAITS.fetch_add(1, Ordering::Relaxed);
                    drivers::drm_device_interface::wait_fence_obj(&f, u64::MAX)
                } {
                    Ok(()) => 0,
                    Err(drivers::DriverError::Busy) => -62,
                    Err(_) => -4,
                },
            };
            if rc != 0 {
                if flags & EXECBUF_FENCE_FD_OUT != 0 {
                    let _ = sched::uaccess::write_user(arg + FENCE_FD_OFF, -1i32);
                }
                return rc;
            }
        }
        if flags & EXECBUF_FENCE_FD_OUT == 0 {
            // Nothing to mint. Forward verbatim — byte-for-byte the pre-change
            // path, so every existing caller (`flags = 0`) is untouched.
            let msg = make_vfs_msg(vfs::VFS_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
            return vfs_reply_val(&vfs::handle(&msg, pid));
        }
        // Reserve the out fence first. EFD_CLOEXEC matches upstream's
        // `get_unused_fd_flags(O_CLOEXEC)` and Mesa's own `os_dupfd_cloexec`:
        // a fence fd must not survive into an exec'd child.
        let efd_msg = make_vfs_msg(vfs::VFS_EVENTFD, &[0u64, EFD_CLOEXEC]);
        let efd = vfs_reply_val(&vfs::handle(&efd_msg, pid));
        if efd < 0 {
            // ATOMIC FAILURE, as upstream: nothing has been submitted, so the
            // caller's command stream is NOT on the host. Mesa's virgl winsys
            // discards the stream on any EXECBUFFER error ("expect bad
            // rendering"), so any CREATE_OBJECT in it is lost and the host
            // later kills the context on the first reference ("Illegal
            // handle"). The error is the caller's to handle; the serial line is
            // so that cause is never again inferred after the fact. Logged for
            // the first 8 and then every power of two, so a client stuck at its
            // fd limit cannot flood the log at frame rate.
            static OUT_FENCE_FD_FAILS: AtomicU64 = AtomicU64::new(0);
            let n = OUT_FENCE_FD_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                drivers::pci::serial_debug("[DRM] EXECBUFFER refused: out-fence fd unavailable, errno=");
                drivers::pci::serial_debug_hex((-efd) as u32);
                drivers::pci::serial_debug(" pid=");
                drivers::pci::serial_debug_hex(pid as u32);
                drivers::pci::serial_debug(" (nothing submitted; total=");
                drivers::pci::serial_debug_hex(n as u32);
                drivers::pci::serial_debug(")\n");
            }
            let _ = sched::uaccess::write_user(arg + FENCE_FD_OFF, -1i32);
            return efd; // EMFILE/ENFILE — reported, never as a silent fence_fd 0
        }
        let msg = make_vfs_msg(vfs::VFS_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
        let rc = vfs_reply_val(&vfs::handle(&msg, pid));
        if rc < 0 {
            // The submission failed, so the fence describes nothing. Release it
            // rather than handing back an fd Mesa will not close (it checks
            // `ret` before touching `fence_fd`), and write -1 so a caller that
            // ignores the return code still cannot close fd 0.
            let close_msg = make_vfs_msg(vfs::VFS_CLOSE, &[efd as u64]);
            let _ = vfs::handle(&close_msg, pid);
            let _ = sched::uaccess::write_user(arg + FENCE_FD_OFF, -1i32);
            return rc;
        }
        // Arm the fd on the open's newest fence — this submission's, or for the
        // fence-only probe, the last real one. The slot reference taken here is
        // dropped by the signaller, so a close() racing the retirement cannot
        // hand the slot to a stranger. If the fence has already retired (or
        // there is none), signal now: the fd is then readable on return, which
        // is exactly the pre-change behaviour for work that is in fact done.
        let open_id = match vfs::vfs_get_node_kind(pid, fd) {
            Some(vfs::VnodeKind::DynamicDevice { open_id, .. }) => open_id,
            _ => 0,
        };
        let fence = drivers::drm_device_interface::open_last_fence(open_id);
        // The DRM layer signals the fd through the VFS, which it cannot name;
        // hand it the seam once, from the one path that arms anything.
        static OUT_FENCE_SIGNALLER_INSTALLED: AtomicBool = AtomicBool::new(false);
        if !OUT_FENCE_SIGNALLER_INSTALLED.swap(true, Ordering::AcqRel) {
            fn out_fence_signal(token: u64) -> bool { vfs::eventfd_slot_signal_unref(token as usize) }
            drivers::drm_device_interface::set_out_fence_signaller(out_fence_signal);
        }
        vfs::eventfd_note(pid, efd as usize, vfs::EVFD_ORIGIN_FENCE, fence, user_backtrace(frame_ptr));
        if let Some(vfs::VnodeKind::EventFd { slot }) = vfs::vfs_get_node_kind(pid, efd as usize) {
            if fence != 0 {
                vfs::eventfd_slot_ref(slot);
                if !drivers::drm_device_interface::out_fence_register(fence, slot as u64) {
                    // Already retired (or the table is full): signal now and
                    // give the reference back ourselves.
                    vfs::eventfd_slot_signal(slot);
                    vfs::eventfd_slot_unref(slot);
                }
            } else {
                vfs::eventfd_slot_signal(slot);
            }
        }
        if !sched::uaccess::write_user(arg + FENCE_FD_OFF, efd as i32) { return -14; }
        return rc;
    }

    // /dev/pipewire (the in-kernel audio server's device): its private
    // commands (0x101 SET_PARAMS, 0x102 GET_DELAY) carry no type byte the
    // filter below knows, so they used to fall through to the TTY server and
    // come back ENOTTY. Route every ioctl on that device to its server, keyed
    // on the fd's device port rather than on command numbers.
    let audio_port = AUDIO_SERVER_PORT.load(Ordering::Relaxed);
    if audio_port != u32::MAX {
        if let Some(vfs::VnodeKind::DynamicDevice { port, .. }) = vfs::vfs_get_node_kind(pid, fd) {
            if port == audio_port {
                let msg = make_vfs_msg(vfs::VFS_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
                let reply = vfs::handle(&msg, pid);
                return u64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8])) as isize;
            }
        }
    }

    // /dev/netctl: its 'N' commands (docs/servers/netctl.md) are answered by
    // the net server's device, keyed on the fd's device port like the audio
    // server above. It copies the argument itself with no lock held.
    if let Some(nc_port) = net_server::netctl::device_port() {
        if let Some(vfs::VnodeKind::DynamicDevice { port, .. }) = vfs::vfs_get_node_kind(pid, fd) {
            if port == nc_port {
                let msg = make_vfs_msg(vfs::VFS_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
                return vfs_reply_val(&vfs::handle(&msg, pid));
            }
        }
    }

    if cmd == FIONREAD || cmd == FBIOGET_VSCREENINFO ||
       cmd == DRM_IOCTL_GET_MODE || cmd == DRM_IOCTL_SET_MODE ||
       cmd == DRM_IOCTL_CREATE_FB || cmd == DRM_IOCTL_FLIP_PAGE ||
       cmd == DRM_IOCTL_SET_PLANE || cmd == DRM_IOCTL_GET_CAPS ||
       cmd == DRM_IOCTL_GPU_IRQ_STATS || cmd == DRM_IOCTL_GPU_PARK_SPIN ||
       cmd == DRM_IOCTL_CTX_DRAIN_US || cmd == 0x100B /* fake HPD uevent, root test hook */ ||
       is_evdev || is_drm ||
       // /dev/uinput ('U'): only on a device-server fd, never on other fd kinds.
       (ioctl_type == 0x55 && matches!(vfs::vfs_get_node_kind(pid, fd), Some(vfs::VnodeKind::DynamicDevice { .. }))) {
        let msg = make_vfs_msg(vfs::VFS_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
        let reply = vfs::handle(&msg, pid);
        return u64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8])) as isize;
    }
    // ── Pseudo-terminals ─────────────────────────────────────────────────────
    // Must come before BOTH the fd<=2 ENOTTY guard and the TTY-server fallback
    // below. The guard is right for a pipe or file on 0/1/2 and wrong for a
    // pty slave, which is a real terminal — and it is exactly where a shell
    // under a terminal emulator has its stdio. The fallback is wrong because
    // the TTY server holds no pty state at all; `pty::ioctl` is the only thing
    // that can answer TIOCGPTN, TIOCSPTLCK or a per-pair termios.
    if let Some(vfs::VnodeKind::Pty { pair, is_master }) = vfs::vfs_get_node_kind(pid, fd) {
        // TIOCGPTPEER returns a *new fd*, so it cannot be answered by the pty
        // pool the way every other command here is — the VFS owns fd tables.
        // Intercepted for the same reason DRM_IOCTL_PRIME_HANDLE_TO_FD is.
        // `arg` is a value (open flags), not a pointer.
        //
        // Answering it at all is what makes cosmic-term able to open a tab:
        // rustix tries this first and only falls back to TIOCGPTN on ENOSYS or
        // EPERM, so the generic ENOTTY below would have failed the open.
        const TIOCGPTPEER: usize = 0x5441;
        if cmd == TIOCGPTPEER {
            return vfs::pty_get_peer(pid, fd, arg as u32);
        }
        // A pair that backs `/dev/ttyN` (VT 2..6) is a virtual console as well
        // as a terminal: the VT/KD set is answered for THAT VT, as Linux does
        // on any VT fd. The two command tables are disjoint (0x56xx/0x4Bxx vs
        // 0x54xx), so nothing a pty answers is shadowed.
        let vt_n = tty_server::pty::vt_of(pair as usize);
        if vt_n != 0 && !is_master && tty_server::vt::owns_ioctl(cmd) {
            let len = tty_server::vt::ioctl_arg_len(cmd);
            if arg != 0 && !validate_user_buf(arg, len) { return -14; }
            return unsafe { tty_server::vt::ioctl(vt_n, cmd, arg) };
        }
        // `pty::ioctl` copies through its argument with the pool lock held,
        // so it is handed a kernel bounce buffer, never the user pointer.
        return tty_ioctl_bounce(cmd, arg, |karg| unsafe {
            tty_server::pty::ioctl(pair as usize, is_master, cmd, karg)
        });
    }

    // ── Virtual consoles ─────────────────────────────────────────────────────
    // After the pty arm above, which returns before this is reached: a pty end
    // is a terminal but not a VT, and `pty::ioctl` answers ENOTTY for the VT/KD
    // set exactly as Linux does. The two command tables are disjoint anyway
    // (VT_* is 0x56xx, KD* is 0x4Bxx, every pty command is 0x54xx), so the
    // ordering is belt and braces rather than the thing that keeps them apart.
    // Before the fd<=2 guard because a session leader's stdio may be a VT, and
    // the guard would answer ENOTTY for it.
    if tty_server::vt::owns_ioctl(cmd) {
        // Pointer-taking commands read/write at most 8 bytes (`struct vt_mode`;
        // `struct vt_stat` is 6), except the keymap string pair (`struct
        // kbsentry`, 513). Validated before `vt::ioctl`, which copies
        // through the pointer — though unlike `pty::ioctl` it holds no lock
        // while doing so, by the contract on its `# Safety` block.
        if arg != 0 && !validate_user_buf(arg, tty_server::vt::ioctl_arg_len(cmd)) { return -14; }
        if let Some(vt) = vfs::fd_vt_number(pid, fd) {
            return unsafe { tty_server::vt::ioctl(vt, cmd, arg) };
        }
        if fd <= 2 && vfs::fd_is_console_stdio(pid, fd) {
            return unsafe { tty_server::vt::ioctl(0, cmd, arg) };  // 0 = active VT
        }
    }

    // Everything left is a terminal ioctl, and a terminal ioctl on something
    // that is not a terminal is ENOTTY. The TTY server answers fd 0/1/2
    // unconditionally from the console termios — correct for the console, but
    // it also made `prog < file` and `prog | other` look interactive, because
    // isatty() *is* TCGETS/TCGETS2 and a success there is the whole test.
    // fd_is_console_stdio is false exactly when 0/1/2 has been redirected onto
    // a real VFS object (file, pipe, directory), and true for the console and
    // for console proxies — including the fd>2 ones the remap above folded to 0.
    if fd <= 2 && !console_proxy && !vfs::fd_is_console_stdio(pid, fd) {
        return ENOTTY;
    }

    // termios/winsize are copied with CONSOLE_TERMIOS held: bounce them.
    tty_ioctl_bounce(cmd, arg, |karg| {
        let msg = make_vfs_msg(tty_server::TTY_IOCTL, &[fd as u64, cmd as u64, karg as u64]);
        net_reply_val(&tty_server::handle(&msg, pid))
    })
}

/// Size of the object a terminal ioctl's `arg` points at, and whether the
/// command writes it (`true`) or reads it (`false`). `None`: `arg` is a value
/// (TCFLSH's selector, TCSBRK's duration, TIOCSCTTY's ignored argument) or
/// the command is not a terminal one (ENOTTY, `arg` unused).
fn tty_ioctl_arg(cmd: usize) -> Option<(usize, bool)> {
    match cmd {
        0x5401 => Some((36, true)),                         // TCGETS
        0x5402 | 0x5403 | 0x5404 => Some((36, false)),      // TCSETS{,W,F}
        0x802C_542A => Some((44, true)),                    // TCGETS2
        0x402C_542B | 0x402C_542C | 0x402C_542D => Some((44, false)), // TCSETS2{,W,F}
        0x5413 => Some((8, true)),                          // TIOCGWINSZ
        0x5414 => Some((8, false)),                         // TIOCSWINSZ
        // TIOCGPGRP, TIOCOUTQ, FIONREAD, TIOCGSID, TIOCGPTN, TIOCGPTLCK
        0x540F | 0x5411 | 0x541B | 0x5429 | 0x8004_5430 | 0x8004_5439 => Some((4, true)),
        // TIOCSPGRP, TIOCSPTLCK (TIOCSIG's arg is the signal number by value)
        0x5410 | 0x4004_5431 => Some((4, false)),
        _ => None,
    }
}

/// Run a terminal ioctl whose handler copies through `arg` with a server
/// lock held (the pty pool, CONSOLE_TERMIOS) against a kernel buffer: the
/// argument is copied in before, and the result out after, through the
/// fault-tolerant uaccess copy with nothing held. A bad pointer is EFAULT;
/// it used to be a kernel-mode fault under the lock, which killed the caller
/// with the lock still held and wedged every other terminal user.
fn tty_ioctl_bounce(cmd: usize, arg: usize, f: impl FnOnce(usize) -> isize) -> isize {
    let (len, out) = match tty_ioctl_arg(cmd) {
        Some(d) => d,
        None => return f(arg),
    };
    if arg == 0 { return -14; } // EFAULT
    let mut kbuf = [0u8; 64];
    if !out && unsafe { sched::uaccess::copy_from_user(kbuf.as_mut_ptr(), arg, len) } != 0 {
        return -14;
    }
    let r = f(kbuf.as_mut_ptr() as usize);
    if out && r >= 0 && unsafe { sched::uaccess::copy_to_user(arg, kbuf.as_ptr(), len) } != 0 {
        return -14;
    }
    r
}

// ── POSIX timer syscalls ──────────────────────────────────────────────────────

/// sys_timer_create(clockid, sigevent_ptr, timerid_ptr)
///
/// The clock is honoured (it used to be ignored: every timer ran on the
/// monotonic clock): CLOCK_REALTIME/MONOTONIC/BOOTTIME (and the _ALARM and
/// TAI variants) count real time — a REALTIME timer's TIMER_ABSTIME values are
/// wall-clock instants that follow clock steps — and
/// CLOCK_PROCESS_CPUTIME_ID / CLOCK_THREAD_CPUTIME_ID count the process's /
/// calling thread's CPU time. The clocks Linux cannot arm a timer on
/// (MONOTONIC_RAW, the _COARSE ones) are EOPNOTSUPP; unknown ids, and the
/// dynamic per-pid CPU clocks (negative ids, unsupported here), EINVAL.
fn sys_timer_create(clockid: usize, sigevent_ptr: usize, timerid_ptr: usize) -> isize {
    match clockid as i32 {
        0 | 1 | 2 | 3 | 7 | 11 => {}
        8 | 9 => if sched::current_euid() != 0 { return -1; } // EPERM: CAP_WAKE_ALARM
        4 | 5 | 6 => return -95, // EOPNOTSUPP
        _ => return -22,
    }
    // struct sigevent: sigev_value(8) + sigev_signo(4) + sigev_notify(4) +
    // sigev_notify_thread_id(4) + ... . We read the first 20 bytes: value,
    // signo, notify, and (for SIGEV_THREAD_ID) the target tid.
    if timerid_ptr != 0 && !validate_user_buf(timerid_ptr, core::mem::size_of::<u32>()) { return -14; }
    // sigev_notify at offset 12: SIGEV_SIGNAL = 0, SIGEV_NONE = 1 (armed,
    // readable via timer_gettime, never signals — encoded as signo 0),
    // SIGEV_THREAD = 2, SIGEV_THREAD_ID = 4 (delivered to the named thread —
    // sigev_notify_thread_id at offset 16 — rather than process-directed, per
    // sigevent(7)).
    //
    // SIGEV_THREAD is a libc construct: glibc and musl create the notify
    // thread themselves and hand the kernel SIGEV_THREAD_ID, and relibc does
    // the same. A raw SIGEV_THREAD reaching the kernel is treated as
    // SIGEV_SIGNAL, as Linux's `good_sigevent` does (it used to be silently
    // turned into SIGEV_NONE: the timer never signalled anything).
    let (signo, sigev_value, target_tid) = if sigevent_ptr != 0 {
        let mut buf = [0u8; 20];
        let ok = validate_user_buf(sigevent_ptr, 20)
            && with_current_address_space(|as_| as_.read_user_buf(sigevent_ptr, &mut buf))
                .unwrap_or(false);
        if !ok { return -14; }
        let value  = u64::from_ne_bytes(buf[0..8].try_into().unwrap());
        let signo  = u32::from_ne_bytes(buf[8..12].try_into().unwrap());
        let notify = u32::from_ne_bytes(buf[12..16].try_into().unwrap());
        match notify {
            1 => (0, 0, 0),
            0 | 2 if (1..=64).contains(&signo) => (signo, value, 0),
            4 if (1..=64).contains(&signo) => {
                let tid = u32::from_ne_bytes(buf[16..20].try_into().unwrap());
                // The target must be a live thread of the caller's group.
                if tid == 0 || !sched::futex_task_exists(tid) || sched::tgid_of(tid) != sched::current_tgid() {
                    return -22;
                }
                (signo, value, tid)
            }
            _ => return -22, // EINVAL
        }
    } else {
        (14, 0, 0) // SIGALRM default
    };
    let pid = current_pid();
    let msg = make_vfs_msg(tty_server::TIMER_CREATE,
        &[signo as u64, timerid_ptr as u64, sigev_value, target_tid as u64,
          clockid as u64, pid as u64]);
    let reply = tty_server::handle(&msg, pid);
    net_reply_val(&reply)
}

/// timer_settime(timerid, flags, new, old): `flags & TIMER_ABSTIME` makes
/// `it_value` an instant on the timer's clock (it used to be ignored, so an
/// absolute deadline was armed as a relative interval of that many seconds).
fn sys_timer_settime(timerid: usize, flags: usize, ispec_ptr: usize, ospec_ptr: usize) -> isize {
    if ispec_ptr != 0 && !validate_user_buf(ispec_ptr, 32) { return -14; }
    if ospec_ptr != 0 && !validate_user_buf(ospec_ptr, 32) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(tty_server::TIMER_SETTIME,
        &[timerid as u64, ispec_ptr as u64, ospec_ptr as u64, flags as u64]);
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
    let h = net_reply_val(&net_server::handle(&msg, pid));
    if h < 0 { return h; }
    publish_handle(pid, h as usize, 0)
}

/// The backing path a pathname `sockaddr_un` names, resolved like every other
/// path the task passes: relative to its cwd, under its chroot, through alias
/// mounts. `Ok(None)` for anything else (another family, an abstract or empty
/// name, a short address), which the net server handles as before.
///
/// The net server used sun_path verbatim, as a host-absolute path, so a
/// chrooted tmux (or ssh-agent, or dbus) bound `/tmp/tmux-0/default` in the
/// host's /tmp, not its own, and a relative name never resolved at all.
fn unix_sockaddr_backing(addr_ptr: usize, addrlen: usize) -> Result<Option<KPath>, isize> {
    const AF_UNIX: u16 = 1;
    const SUN_PATH_MAX: usize = 108;
    if addrlen < 3 { return Ok(None); }
    let mut raw = [0u8; 2 + SUN_PATH_MAX];
    let n = addrlen.min(raw.len());
    let ok = with_current_address_space(|as_| as_.read_user_buf(addr_ptr, &mut raw[..n])).unwrap_or(false);
    if !ok { return Err(-14); }
    if u16::from_ne_bytes([raw[0], raw[1]]) != AF_UNIX || raw[2] == 0 { return Ok(None); }
    let name = &raw[2..n];
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    Ok(Some(KPath::from_ns(resolve_ns(name)?)?))
}

/// NET_BIND / NET_CONNECT for `sockfd`, carrying the resolved pathname (see
/// `unix_sockaddr_backing`) in args 3-4.
fn net_addr_call(tag: u64, sockfd: usize, addr_ptr: usize, addrlen: usize) -> isize {
    let host = match unix_sockaddr_backing(addr_ptr, addrlen) { Ok(h) => h, Err(e) => return e };
    let (hp, hl) = match &host { Some(k) => (k.ptr() as u64, k.bytes().len() as u64), None => (0, 0) };
    let msg = make_vfs_msg(tag, &[sockfd as u64, addr_ptr as u64, addrlen as u64, hp, hl]);
    let r = net_reply_val(&net_server::handle(&msg, current_pid()));
    drop(host);
    r
}

fn sys_bind(sockfd: usize, addr_ptr: usize, addrlen: usize) -> isize {
    if addrlen > 128 || !validate_user_buf(addr_ptr, addrlen) { return -14; }
    net_addr_call(net_server::NET_BIND, sockfd, addr_ptr, addrlen)
}

fn sys_listen(sockfd: usize, backlog: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_LISTEN, &[sockfd as u64, backlog as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_accept(sockfd: usize, addr_ptr: usize, addrlen_ptr: usize, flags: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_ACCEPT,
        &[sockfd as u64, addr_ptr as u64, addrlen_ptr as u64, flags as u64]);
    // A blocking listener waits for a connection (bounded by SO_RCVTIMEO);
    // it used to answer EAGAIN at once, like a non-blocking one.
    let r = net_block_lazy(pid, sockfd, false, false,
        || net_reply_val(&net_server::handle(&msg, pid)));
    if r < 0 { return r; }
    let r = publish_handle(pid, r as usize, 0);
    if r >= 0 { uxtrace("ACC", pid, sockfd, r); }
    r
}

fn sys_connect(sockfd: usize, addr_ptr: usize, addrlen: usize) -> isize {
    if addrlen > 128 || !validate_user_buf(addr_ptr, addrlen) { return -14; }
    let pid = current_pid();
    let r = net_addr_call(net_server::NET_CONNECT, sockfd, addr_ptr, addrlen);
    uxtrace("CON", pid, sockfd, r);
    // A repeat connect() while the first one's outcome is unread: Linux
    // answers EALREADY only while the handshake is in flight, then 0 once
    // established and the pending error once refused. Without this, a
    // caller that retries connect() instead of reading SO_ERROR saw
    // EALREADY for good, since a non-blocking connect now always leaves
    // its outcome pending.
    if r == -114 && net_server::tcp_connect_pending(pid, sockfd) {
        let st = net_server::tcp_connect_status(pid, sockfd);
        return if st == -11 { -114 } else { st };
    }
    if r != 0 { return r; }
    // A TCP connect has only queued its SYN. Linux semantics from here: a
    // non-blocking socket answers EINPROGRESS and learns the outcome from
    // poll + SO_ERROR; a blocking one waits for the handshake and returns 0
    // or the error. Returning 0 at once, as this used to, told every caller
    // it was connected before anything had answered, and a refused connect
    // then looked like a connection that closed without a word.
    // Non-blocking: EINPROGRESS for every TCP connect, whatever the handshake
    // has done by now, as Linux answers (the socket lock holds the loopback
    // SYN-ACK/RST in the backlog until connect() returns). Asking for the
    // status here raced the loopback stack on another CPU: an RST processed
    // first made connect() fail at once with ECONNREFUSED and consumed the
    // pending error, so poll saw no POLLERR and SO_ERROR read 0 (scmtest
    // tcp_connect_refused, ~1 run in 5-10). The outcome stays pending for
    // poll + SO_ERROR.
    if net_fd_nonblock(pid, sockfd) {
        return if net_server::tcp_connect_pending(pid, sockfd) { -115 } else { 0 }; // EINPROGRESS
    }
    let st = net_server::tcp_connect_status(pid, sockfd);
    if st != -11 { return st; }
    // Linux gives up on an unanswered SYN after ~2 min (6 retries); 75 s is
    // the classic BSD connect timeout.
    let now = sched::monotonic_ns();
    let deadline = now + 75_000_000_000;
    // SO_SNDTIMEO bounds the wait too; when it runs out first the handshake
    // carries on and the answer is EINPROGRESS (Linux's inet_stream_connect).
    let timeo = net_server::sock_timeo(pid, sockfd, true);
    let user_deadline = if timeo == 0 { u64::MAX } else { now.saturating_add(timeo) };
    let r = block_until_ready(false, || {
        let st = net_server::tcp_connect_status(pid, sockfd);
        if st != -11 { return st; }
        let t = sched::monotonic_ns();
        if t >= user_deadline { SOCK_TIMED_OUT }
        else if t >= deadline { -110 } // ETIMEDOUT
        else { st }
    }, || {
        // The socket's own tag parks with no deadline: wake for the timeout.
        let (mask, d) = net_block_hint(pid, sockfd);
        (mask, d.min(deadline).min(user_deadline))
    });
    // Interrupted: EINTR, and the handshake carries on as on Linux. Restarting
    // the call would only answer EALREADY.
    if r == ERESTARTSYS { return -4; }
    if r == SOCK_TIMED_OUT { return -115; }
    r
}


/// Blocking retry for a read/write/send/recv that answered EAGAIN on a
/// blocking fd — the replacement for the `irq_window(); yield_now(..)` spins
/// that used to sit here.
///
/// Those spins kept a CPU 100 % busy for as long as a blocking read()er waited
/// on an empty pipe or socket: greetd's PAM worker alone (recvfrom on a
/// socketpair) pinned one host core for the life of the greeter, and every
/// `cmd | cmd` pipeline with a slow side pinned another (2026-09-16).
///
/// Three-phase park on the poll wait-channel, exactly as poll()/epoll_wait do:
/// publish Blocked with the fd's interest tag, re-issue the operation (a
/// producer that changed the level between the first EAGAIN and the publish is
/// caught here, and its wake — which found us already Blocked — makes the
/// commit a no-op yield), then yield. The wake that releases us is the one a
/// poll(POLLIN/POLLOUT) waiter on the same fd relies on, so the guarantee is
/// no weaker than poll's. `hint` gives `(mask, deadline)`: a kind whose
/// producers are trusted parks on its tag with no deadline; one without parks
/// on the broadcast mask with a one-tick deadline — a 100 Hz re-probe, never a
/// spin.
fn block_until_ready(nonblock: bool, mut op: impl FnMut() -> isize, hint: impl Fn() -> (sched::PollTag, u64)) -> isize {
    loop {
        let n = op();
        if n != -11 || nonblock { return n; }
        if interrupted() { return ERESTARTSYS; }
        let gen0 = sched::poll_retag_gen();
        let (tag, deadline) = hint();
        // Keyed / foreign answers park refined, as poll() on the fd would.
        sched::block_on_poll_prepare_tag(deadline, &tag);
        let n = op();
        if n != -11 { sched::block_on_poll_cancel(); return n; }
        if interrupted() { sched::block_on_poll_cancel(); return ERESTARTSYS; }
        // The fd's tag moved with its state since `hint` (a socket bound or
        // connected by another thread): the mask may name only the old keys,
        // already woken. Take the new tag instead of parking on a stale one.
        if sched::poll_retag_gen() != gen0 { sched::block_on_poll_cancel(); continue; }
        sched::block_on_poll_commit();
    }
}

/// `block_until_ready` hint for a VFS fd: its producer tag, or broadcast + one
/// tick when the kind has no trusted tagged producer.
fn vfs_block_hint(pid: u32, fd: usize, write: bool) -> (sched::PollTag, u64) {
    match vfs::fd_wake_tag(pid, fd, write) {
        Some(tag) => (tag, u64::MAX),
        None => (sched::PollTag::ALL, sched::monotonic_ns() + 10_000_000),
    }
}

/// `block_until_ready` hint for a socket fd (see `net_server::fd_wake_tag`).
fn net_block_hint(pid: u32, fd: usize) -> (sched::PollTag, u64) {
    match net_server::fd_wake_tag(pid, fd) {
        Some(tag) => (tag, u64::MAX),
        None => (sched::PollTag::ALL, sched::monotonic_ns() + 10_000_000),
    }
}

/// Internal answer of a socket wait whose SO_RCVTIMEO/SO_SNDTIMEO ran out;
/// never returned to user space.
const SOCK_TIMED_OUT: isize = -0x1_0000;

/// `block_until_ready` for a socket operation, bounded by the socket's
/// SO_SNDTIMEO (`send`) or SO_RCVTIMEO the way Linux's sock_sndtimeo /
/// sock_rcvtimeo bound it: EAGAIN when the time runs out, and with a timeout
/// set a signal is EINTR, never a restart (sock_intr_errno). Without one a
/// signal is ERESTARTSYS, which SA_RESTART restarts. The timeout is looked up
/// only once the operation has answered EAGAIN, so a call that never waits
/// pays nothing for it.
fn net_block_timeo(pid: u32, fd: usize, nonblock: bool, send: bool, mut op: impl FnMut() -> isize) -> isize {
    let n = op();
    if n != -11 || nonblock { return n; }
    let timeo = net_server::sock_timeo(pid, fd, send);
    if timeo == 0 { return block_until_ready(false, op, || net_block_hint(pid, fd)); }
    let deadline = sched::monotonic_ns().saturating_add(timeo);
    let r = block_until_ready(false, || {
        let n = op();
        if n == -11 && sched::monotonic_ns() >= deadline { SOCK_TIMED_OUT } else { n }
    }, || {
        let (tag, d) = net_block_hint(pid, fd);
        (tag, d.min(deadline))
    });
    match r {
        SOCK_TIMED_OUT => -11,
        ERESTARTSYS => -4,
        r => r,
    }
}

/// Shared blocking wrapper for the four send/recv syscalls: a blocking
/// socket loops on EAGAIN (EINTR-aware, bounded by SO_SNDTIMEO for a `send`,
/// SO_RCVTIMEO otherwise), a nonblocking one — O_NONBLOCK on the fd or
/// MSG_DONTWAIT in `flags` — returns it straight through.
fn net_blocking_op(pid: u32, sockfd: usize, flags: usize, send: bool, msg: &Message) -> isize {
    const MSG_DONTWAIT: usize = 0x40;
    net_block_lazy(pid, sockfd, flags & MSG_DONTWAIT != 0, send, || net_reply_val(&net_server::handle(msg, pid)))
}

/// `net_block_timeo` that asks for the fd's O_NONBLOCK (a whole NET_GETFL
/// round through the net server, SOCK_TABLES included) only when the first
/// attempt would block: an op that completes at once never needs it (lane
/// ipcfast: one GETFL per send/recv was a third of a 64 B round trip's
/// lock traffic). `dontwait`: MSG_DONTWAIT or the like, known up front.
fn net_block_lazy(pid: u32, fd: usize, dontwait: bool, send: bool, mut op: impl FnMut() -> isize) -> isize {
    let n = op();
    if n != -11 || dontwait || net_fd_nonblock(pid, fd) { return n; }
    net_block_timeo(pid, fd, false, send, op)
}

/// M4/M5 unix-socket exchange trace. Prints one line per connect/accept and
/// per data-carrying send/recv to the serial console (the driver.py-captured
/// port), so a compositor↔client handshake stall can be localized without
/// touching the net-server crate (which has no serial access). Quiet gate:
/// flip UXTRACE to false to silence with zero cost. Kept for M5 (cosmic-comp
/// exercises the identical path). Values print in hex; negatives are shown as
/// two's-complement (e.g. EAGAIN -11 = ...fffffff5), but data sends/recvs only
/// trace on v>0 so the stream stays readable.
const UXTRACE: bool = false;

#[inline]
fn uxtrace(tag: &str, pid: u32, fd: usize, v: isize) {
    if !UXTRACE { return; }
    crate::serial_print_str("UXTR ");
    crate::serial_print_str(tag);
    crate::serial_print_str(" pid=");
    crate::serial_print_hex(pid as usize);
    crate::serial_print_str(" fd=");
    crate::serial_print_hex(fd);
    crate::serial_print_str(" v=");
    crate::serial_print_hex(v as usize);
    crate::serial_print_str("\n");
}

fn sys_sendto(sockfd: usize, buf_ptr: usize, len: usize,
              flags: usize, addr_ptr: usize, addrlen: usize) -> isize {
    if len != 0 && !validate_user_buf(buf_ptr, len) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SEND,
        &[sockfd as u64, buf_ptr as u64, len as u64,
          flags as u64, addr_ptr as u64, addrlen as u64]);
    net_blocking_op(pid, sockfd, flags, true, &msg)
}

fn sys_recvfrom(sockfd: usize, buf_ptr: usize, len: usize,
                flags: usize, addr_ptr: usize, addrlen_ptr: usize) -> isize {
    if len != 0 && !validate_user_buf(buf_ptr, len) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_RECV,
        &[sockfd as u64, buf_ptr as u64, len as u64,
          flags as u64, addr_ptr as u64, addrlen_ptr as u64]);
    net_blocking_op(pid, sockfd, flags, false, &msg)
}

fn sys_sendmsg(sockfd: usize, msghdr_ptr: usize, flags: usize) -> isize {
    if !validate_user_buf(msghdr_ptr, 56) { return -14; } // struct msghdr (msg_flags at +48)
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SENDMSG,
        &[sockfd as u64, msghdr_ptr as u64, flags as u64]);
    let r = net_blocking_op(pid, sockfd, flags, true, &msg);
    if r > 0 { uxtrace("SND", pid, sockfd, r); }
    r
}

fn sys_recvmsg(sockfd: usize, msghdr_ptr: usize, flags: usize) -> isize {
    if !validate_user_buf(msghdr_ptr, 56) { return -14; } // struct msghdr (msg_flags at +48)
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_RECVMSG,
        &[sockfd as u64, msghdr_ptr as u64, flags as u64]);
    let r = net_blocking_op(pid, sockfd, flags, false, &msg);
    if r > 0 { uxtrace("RCV", pid, sockfd, r); }
    r
}

/// `struct mmsghdr`: a `struct msghdr` (56 bytes) then `msg_len` (u32),
/// padded to 64.
const MMSGHDR_SIZE: usize = 64;
const MMSG_LEN_OFF: usize = 56;
/// Linux UIO_MAXIOV: the most messages one sendmmsg/recvmmsg moves.
const MMSG_MAX: usize = 1024;

/// sendmmsg(2): `sendmsg` for each of up to `vlen` messages, each one's byte
/// count in its `msg_len`. Answers how many were sent; an error on the first
/// is the call's error, a later one ends the call short (Linux). quinn-udp
/// (QUIC: neqo, i.e. Firefox's HTTP/3 on its own sockets) batches with these.
fn sys_sendmmsg(sockfd: usize, vec_ptr: usize, vlen: usize, flags: usize) -> isize {
    let vlen = vlen.min(MMSG_MAX);
    for i in 0..vlen {
        let h = vec_ptr + i * MMSGHDR_SIZE;
        if !validate_user_buf(h, MMSGHDR_SIZE) { return if i > 0 { i as isize } else { -14 }; }
        let r = sys_sendmsg(sockfd, h, flags);
        if r < 0 { return if i > 0 { i as isize } else { r }; }
        if !sched::uaccess::write_user::<u32>(h + MMSG_LEN_OFF, r as u32) {
            return if i > 0 { i as isize } else { -14 };
        }
    }
    vlen as isize
}

/// recvmmsg(2): `recvmsg` into each of up to `vlen` messages. Every message
/// after the first is MSG_DONTWAIT under MSG_WAITFORONE; without it a
/// blocking socket waits for all `vlen`. The timeout, as on Linux, is only
/// checked after each message received. Answers how many were received; an
/// error on the first is the call's error, a later one (EAGAIN) ends it short.
fn sys_recvmmsg(sockfd: usize, vec_ptr: usize, vlen: usize, flags: usize, timeout_ptr: usize) -> isize {
    const MSG_DONTWAIT: usize = 0x40;
    const MSG_WAITFORONE: usize = 0x10000;
    let deadline = if timeout_ptr != 0 {
        let (s, ns) = match (sched::uaccess::read_user::<i64>(timeout_ptr), sched::uaccess::read_user::<i64>(timeout_ptr + 8)) {
            (Some(s), Some(ns)) => (s, ns),
            _ => return -14,
        };
        if s < 0 || !(0..1_000_000_000).contains(&ns) { return -22; }
        Some(monotonic_ns().saturating_add((s as u64).saturating_mul(1_000_000_000)).saturating_add(ns as u64))
    } else { None };
    let vlen = vlen.min(MMSG_MAX);
    let base = flags & !MSG_WAITFORONE;
    let mut n = 0usize;
    while n < vlen {
        let h = vec_ptr + n * MMSGHDR_SIZE;
        if !validate_user_buf(h, MMSGHDR_SIZE) { return if n > 0 { n as isize } else { -14 }; }
        let f = if n > 0 && flags & MSG_WAITFORONE != 0 { base | MSG_DONTWAIT } else { base };
        let r = sys_recvmsg(sockfd, h, f);
        if r < 0 { return if n > 0 { n as isize } else { r }; }
        if !sched::uaccess::write_user::<u32>(h + MMSG_LEN_OFF, r as u32) {
            return if n > 0 { n as isize } else { -14 };
        }
        n += 1;
        if deadline.map_or(false, |d| monotonic_ns() >= d) { break; }
    }
    n as isize
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
    let r = net_reply_val(&net_server::handle(&msg, pid));
    if r < 0 { return r; }
    // The net server wrote the two socket handles; replace them with
    // descriptors. Either failing closes both.
    let (ha, hb) = match (sched::uaccess::read_user::<u32>(sv_ptr), sched::uaccess::read_user::<u32>(sv_ptr + 4)) {
        (Some(a), Some(b)) => (a as usize, b as usize),
        _ => return -14,
    };
    let fa = publish_handle(pid, ha, 0);
    if fa < 0 { close_handle(pid, hb); return fa; }
    let fb = publish_handle(pid, hb, 0);
    if fb < 0 { let _ = sys_close(fa as usize); return fb; }
    let sv = [fa as u32, fb as u32];
    let ok = with_current_address_space_mut(|as_| {
        as_.write_user_buf(sv_ptr, unsafe { core::slice::from_raw_parts(sv.as_ptr() as *const u8, 8) })
    }).unwrap_or(false);
    if !ok { let _ = sys_close(fa as usize); let _ = sys_close(fb as usize); return -14; }
    0
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

// K2 caps: one instance per {compositor, busd, each client, tokio runtime};
// the compositor watches every client fd + input + timers on one instance.
// 64→256 (m7z2): GLOBAL, session-wide pool. The `polling` backend couples an
// epoll instance 1:1 with an eventfd waker + a timerfd (see vfs MAX_EVENTFDS),
// so #epoll-instances tracks #event-loops across a full ~20-process COSMIC
// session (~150+ with tokio + calloop + per-notify::Watcher Polls). At 64 it
// exhausted alongside the eventfd pool. Each EpollInstance is ~16 KB (512
// interests), so 256 = ~4 MB of zero-init .bss (NOBITS — no ELF/file cost, only
// runtime RAM, abundant in the guest). MAX_EPOLL_INTERESTS stays 512 (the
// compositor's single instance watches every client fd + input + timers).
// 256 -> 1024 (lane multiterm, 2026-09-27): each cosmic-term adds ~11
// instances (tokio workers, calloop, wgpu), and five terminals took the pool
// to 177/256. An empty instance is now all-zero bytes (see
// `EpollInterest::empty`), so the ~20 MB table is .bss, not kernel image.
//
// Fixed tables -> grown on demand (lane fdmax): the instance table, each
// instance's interest list and the epoll fd table are heap vectors that grow
// as they are used, up to these ceilings. A process watching thousands of
// fds (relibc's poll() is one epoll instance over the whole pollfd array) is
// bounded by its RLIMIT_NOFILE, and an idle system no longer carries ~20 MiB
// of zeroed instances.
const MAX_EPOLL_INSTANCES: usize = 1 << 20;
const MAX_EPOLL_INTERESTS: usize = 1 << 20;

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
    /// EPOLLONESHOT bookkeeping: cleared after the interest fires once, so it
    /// stays quiet until an `epoll_ctl(MOD)` re-arms it. Interests without
    /// EPOLLONESHOT stay permanently armed. `true` for a fresh/ADD'd interest.
    armed: bool,
    /// The open file description `fd` named at ADD (`vfs::ofd`). The item's
    /// key is (fd, ofd), as Linux's is (file, fd): it lives until the
    /// description's last reference anywhere is gone (`epoll_ofd_released`),
    /// not until `fd` is closed. 0 = none (an untracked console fd, a nested
    /// epoll fd, a full description table): keyed by number, as before.
    ofd: u32,
    /// Where the description is probed once `fd` in the owner no longer names
    /// it (closed while a dup, a fork child or an SCM_RIGHTS receiver keeps
    /// it): that process and its fd. `ptgid` 0 = the owner's own `fd`.
    ptgid: u32,
    pfd: u32,
    /// `EPOLL_SEQ` value of this registration's last ADD or MOD: an epoll_wait
    /// snapshot (`EpCache`) that saw another value re-probes it (lane
    /// epollready).
    mseq: u64,
}

impl EpollInterest {
    /// All-zero on purpose, so `EPOLL_INSTANCES` is zero-initialised (.bss).
    /// Every reader skips `!in_use` entries, and CTL_ADD writes all fields
    /// (last_seq = u64::MAX, armed = true), so these values are never seen.
    const fn empty() -> Self {
        Self { fd: 0, events: 0, data: 0, in_use: false, last_seq: 0, armed: false, ofd: 0, ptgid: 0, pfd: 0, mseq: 0 }
    }

    fn is(&self, fd: i32, ofd: u32) -> bool { self.in_use && self.fd == fd && self.ofd == ofd }
}

#[derive(Clone)]
struct EpollInstance {
    owner_pid: u32,
    /// TGID of the creating thread, captured at `epoll_create1`.
    ///
    /// Every ownership check wants the *thread group*, and resolving it from
    /// `owner_pid` meant `sched::tgid_of()` — the global `RUN_QUEUE` lock plus
    /// a 256-slot linear scan — on every `epoll_wait`/`epoll_ctl`. That was
    /// 10.4% of running guest CPU on a live COSMIC session.
    ///
    /// Caching it is also strictly more correct than resolving it: `tgid_of`
    /// falls back to the pid itself when the task is gone, so once the thread
    /// that called `epoll_create1` exited, its still-open epoll fd started
    /// failing EBADF for every surviving sibling — exactly the thread-scoped
    /// behaviour the checks were written to avoid. A thread group's id is
    /// fixed for its lifetime, so the captured value stays valid.
    owner_tgid: u32,
    /// Registrations; grows on ADD when no slot is free (`hi` <= len).
    interests: alloc::vec::Vec<EpollInterest>,
    in_use:    bool,
    /// Number of epoll fds (EPOLL_FDS entries) referencing this instance.
    /// fcntl(F_DUPFD*) on an epoll fd creates a second fd aliasing the same
    /// instance (mio/tokio clone their registry handle this way); the
    /// instance is only torn down when the last alias closes.
    refs:      u32,
    /// High-water mark: one past the highest interest slot ever used by this
    /// instance, so the probe loops can stop there instead of walking all
    /// `MAX_EPOLL_INTERESTS`.
    ///
    /// Monotonic within an instance's lifetime — `CTL_DEL` deliberately does
    /// not shrink it. A slot freed by DEL can be reused by the next ADD, so a
    /// shrinking mark would have to rescan to find the new maximum, and every
    /// consumer only ever uses it as an upper bound over `in_use` slots. It
    /// resets to 0 with the rest of the instance via [`EpollInstance::empty`].
    ///
    /// Why it matters: both probe loops ran `0..MAX_EPOLL_INTERESTS` (512)
    /// unconditionally, taking the global `EPOLL_INSTANCES` spinlock once per
    /// slot, for instances that typically watch a handful of fds. On a live
    /// COSMIC session that put `sys_epoll_wait` + `epoll_any_ready_nested` at
    /// 22.3% of all running guest CPU.
    hi:        u32,
    /// `EPOLL_SEQ` value at creation: tells an `EpCache` that its slot now
    /// holds another instance.
    id:        u64,
}

impl EpollInstance {
    const fn empty() -> Self {
        Self { owner_pid: 0, owner_tgid: 0,
               interests: alloc::vec::Vec::new(),
               in_use: false, refs: 0, hi: 0, id: 0 }
    }
}

/// Source of `EpollInstance::id` and `EpollInterest::mseq` (lane epollready).
static EPOLL_SEQ: AtomicU64 = AtomicU64::new(1);
fn epoll_next_seq() -> u64 { EPOLL_SEQ.fetch_add(1, Ordering::Relaxed) }

/// Epoll handles are an indirection over instance slots so that two fds
/// can alias one instance (dup semantics). handle = EPOLL_FD_BASE + entry
/// index. Since lane fdmax a handle is not a user-visible fd: the process's
/// fd names it through a `vfs::VnodeKind::EpollFd` entry, and the syscall
/// layer translates (`fd_handle`). The handle range lies above every fd
/// number (`vfs::FD_NUMBER_CEIL`) and above the socket handles.
const MAX_EPOLL_FDS: usize = 1 << 24;
const _: () = assert!(EPOLL_FD_BASE >= net_server::SOCK_FD_END);
const _: () = assert!(EPOLL_FD_BASE + MAX_EPOLL_FDS <= HANDLE_SPACE_END);

/// One epoll fd number. An epoll fd is an open file description like any
/// other: fork gives the child the same number naming the same instance (both
/// see one interest list), and each process's copy carries its own
/// FD_CLOEXEC. The numbers are global (`EPOLL_FD_BASE + index`), so a forked
/// child's copy is the same entry with a second *holder*: the primary holder
/// lives inline (`tgid`, `cloexec` — the only one in the common unforked
/// case, so the per-call lookup stays one compare), any further ones in
/// `EpollFdTable::extra`. The entry, and the reference it holds on its
/// instance (`EpollInstance::refs`), goes when its last holder closes it.
#[derive(Clone, Copy)]
struct EpollFdEntry {
    in_use: bool,
    slot: u32,
    /// Primary holder's FD_CLOEXEC (EPOLL_CLOEXEC, F_DUPFD_CLOEXEC, F_SETFD,
    /// close_range's CLOSE_RANGE_CLOEXEC): closed by execve (`epoll_exec_cloexec`).
    cloexec: bool,
    /// Primary holder (thread group). Never 0 while `in_use`.
    tgid: u32,
    /// Number of further holders of this entry in `EpollFdTable::extra`.
    extra: u16,
}

impl EpollFdEntry {
    const EMPTY: Self = Self { in_use: false, slot: 0, cloexec: false, tgid: 0, extra: 0 };
}

/// A further holder of an epoll fd number (a fork child's copy).
#[derive(Clone, Copy)]
struct EpollFdHold { idx: u32, tgid: u32, cloexec: bool }

struct EpollFdTable {
    /// Grows (never shrinks) when no entry is free; an index past the end is
    /// simply not open.
    t: alloc::vec::Vec<EpollFdEntry>,
    extra: alloc::vec::Vec<EpollFdHold>,
}

impl EpollFdTable {
    /// `tgid`'s FD_CLOEXEC on entry `i`, or None when `tgid` does not hold it.
    fn held(&self, i: usize, tgid: u32) -> Option<bool> {
        let e = self.t.get(i)?;
        if !e.in_use { return None; }
        if e.tgid == tgid { return Some(e.cloexec); }
        if e.extra == 0 { return None; }
        self.extra.iter().find(|h| h.idx as usize == i && h.tgid == tgid).map(|h| h.cloexec)
    }

    /// The instance slot behind number index `i`, if `tgid` holds it.
    fn slot_for(&self, i: usize, tgid: u32) -> Option<usize> {
        self.held(i, tgid).map(|_| self.t[i].slot as usize)
    }

    fn set_cloexec(&mut self, i: usize, tgid: u32, v: bool) {
        if i >= self.t.len() { return; }
        if self.t[i].tgid == tgid { self.t[i].cloexec = v; return; }
        if let Some(h) = self.extra.iter_mut().find(|h| h.idx as usize == i && h.tgid == tgid) { h.cloexec = v; }
    }

    /// A fresh entry for `slot`, held by `tgid` (the caller takes the
    /// instance reference). None: the table is full.
    fn alloc(&mut self, slot: usize, tgid: u32, cloexec: bool) -> Option<usize> {
        let i = match self.t.iter().position(|e| !e.in_use) {
            Some(i) => i,
            None => {
                if self.t.len() >= MAX_EPOLL_FDS || self.t.try_reserve(1).is_err() { return None; }
                self.t.push(EpollFdEntry::EMPTY);
                self.t.len() - 1
            }
        };
        self.t[i] = EpollFdEntry { in_use: true, slot: slot as u32, cloexec, tgid, extra: 0 };
        Some(i)
    }

    /// Give `tgid` a copy of entry `i` (fork).
    fn add_holder(&mut self, i: usize, tgid: u32, cloexec: bool) {
        if self.held(i, tgid).is_some() { return; }
        self.extra.push(EpollFdHold { idx: i as u32, tgid, cloexec });
        self.t[i].extra += 1;
    }

    /// Drop `tgid`'s copy of entry `i`. Returns Some(slot) when that was the
    /// last holder (the entry is freed and its instance reference must go),
    /// Some(usize::MAX) when other holders remain, None when `tgid` held none.
    fn drop_holder(&mut self, i: usize, tgid: u32) -> Option<usize> {
        if i >= self.t.len() || !self.t[i].in_use { return None; }
        if self.t[i].tgid == tgid {
            if self.t[i].extra == 0 {
                let slot = self.t[i].slot as usize;
                self.t[i] = EpollFdEntry::EMPTY;
                return Some(slot);
            }
            // Promote another holder to primary.
            let k = self.extra.iter().position(|h| h.idx as usize == i)?;
            let h = self.extra.swap_remove(k);
            self.t[i].tgid = h.tgid;
            self.t[i].cloexec = h.cloexec;
            self.t[i].extra -= 1;
            return Some(usize::MAX);
        }
        if self.t[i].extra == 0 { return None; }
        let k = self.extra.iter().position(|h| h.idx as usize == i && h.tgid == tgid)?;
        self.extra.swap_remove(k);
        self.t[i].extra -= 1;
        Some(usize::MAX)
    }

    /// Every entry index `tgid` holds, with its FD_CLOEXEC.
    fn held_by(&self, tgid: u32) -> alloc::vec::Vec<(usize, bool)> {
        let mut v: alloc::vec::Vec<(usize, bool)> = self.t.iter().enumerate()
            .filter(|(_, e)| e.in_use && e.tgid == tgid).map(|(i, e)| (i, e.cloexec)).collect();
        v.extend(self.extra.iter().filter(|h| h.tgid == tgid).map(|h| (h.idx as usize, h.cloexec)));
        v
    }

    /// Some holder of any number on instance `slot` (to re-home an instance
    /// whose creator is gone).
    fn any_holder_of(&self, slot: usize) -> Option<u32> {
        for (i, e) in self.t.iter().enumerate() {
            if e.in_use && e.slot as usize == slot {
                if e.tgid != 0 { return Some(e.tgid); }
                if let Some(h) = self.extra.iter().find(|h| h.idx as usize == i) { return Some(h.tgid); }
            }
        }
        None
    }
}

static EPOLL_FDS: spin::Mutex<EpollFdTable> =
    spin::Mutex::new(EpollFdTable { t: alloc::vec::Vec::new(), extra: alloc::vec::Vec::new() });

fn epoll_fd_index(epfd: usize) -> Option<usize> {
    if (EPOLL_FD_BASE..EPOLL_FD_BASE + MAX_EPOLL_FDS).contains(&epfd) { Some(epfd - EPOLL_FD_BASE) } else { None }
}

/// Resolve an epoll fd to its instance slot, or None if out of range/closed
/// (by anyone). Readiness/tag helpers that do not act for a caller use this;
/// everything a caller does through the number goes via `epoll_slot_for`.
fn epoll_slot_of(epfd: usize) -> Option<usize> {
    let i = epoll_fd_index(epfd)?;
    let t = EPOLL_FDS.lock();
    let e = *t.t.get(i)?;
    if e.in_use { Some(e.slot as usize) } else { None }
}

/// Resolve an epoll fd to its instance slot for thread group `tgid`: None
/// (EBADF) unless `tgid` holds the number — it created it, dup'd it, or
/// inherited it across fork.
fn epoll_slot_for(epfd: usize, tgid: u32) -> Option<usize> {
    let i = epoll_fd_index(epfd)?;
    EPOLL_FDS.lock().slot_for(i, tgid)
}

/// fork/vfork: the child (`child`, its own thread group) gets a copy of
/// every epoll fd `parent` holds, naming the same instance — an epoll fd is
/// an open file description, shared across fork like any other. The
/// interests stay as they are: they name the parent's fds/descriptions,
/// which the child's inherited fds share.
/// fork/vfork/clone without CLONE_THREAD: give the child copies of the
/// parent's per-process tables — fd table, socket table, epoll holds — before
/// it is enqueued. The child's fd table names every socket and epoll fd the
/// parent's does; the net server does not copy inet and listening sockets,
/// so their descriptors are dropped from the child's table here. (All three
/// fork paths go through this one function so none can miss a step.)
fn fork_dup_tables(parent_pid: u32, child_pid: u32) {
    let msg = make_vfs_msg(vfs::VFS_FORK_DUP, &[parent_pid as u64, child_pid as u64]);
    let _ = vfs::handle(&msg, parent_pid);
    let nmsg = make_vfs_msg(net_server::NET_FORK_DUP, &[parent_pid as u64, child_pid as u64]);
    let _ = net_server::handle(&nmsg, parent_pid);
    epoll_fork_dup(parent_pid, child_pid);
    stdio_flags_fork(sched::tgid_of(parent_pid), child_pid);
    vfs::prune_handles(child_pid, |h| handle_alive(child_pid, h));
    // The net and epoll tables are per process, so the copies above took a
    // reference on every handle of the parent's PROCESS, but the child only
    // inherited the fd table of the forking thread (a thread that unshared
    // its table has a different one). Release what the child cannot name.
    let named = vfs::table_handles(child_pid);
    for fd in net_server::open_sock_fds(child_pid) {
        if !named.contains(&fd) {
            let m = make_vfs_msg(net_server::NET_CLOSE, &[fd as u64]);
            let _ = net_server::handle(&m, child_pid);
        }
    }
    let held = { EPOLL_FDS.lock().held_by(child_pid) };
    for (i, _) in held {
        if !named.contains(&(EPOLL_FD_BASE + i)) { let _ = epoll_fd_drop(i, child_pid); }
    }
}

fn epoll_fork_dup(parent: u32, child: u32) {
    let mut t = EPOLL_FDS.lock();
    for (i, cloexec) in t.held_by(parent) { t.add_holder(i, child, cloexec); }
}

/// fcntl on an epoll fd. Supports the dup commands mio/tokio actually use;
/// flag commands are accepted as no-ops (epoll fds carry no meaningful
/// status flags here).
fn epoll_fcntl(epfd: usize, cmd: usize, arg: usize) -> isize {
    const F_DUPFD: usize = 0;
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_DUPFD_CLOEXEC: usize = 1030;
    let tgid = sched::current_tgid();
    let i = match epoll_fd_index(epfd) { Some(i) => i, None => return -9 };
    match cmd {
        F_GETFD | F_SETFD => {
            let mut t = EPOLL_FDS.lock();
            let c = match t.held(i, tgid) { Some(c) => c, None => return -9 };
            if cmd == F_SETFD { t.set_cloexec(i, tgid, arg & 1 != 0); 0 } else { c as isize }
        }
        F_DUPFD | F_DUPFD_CLOEXEC => {
            if arg >= vfs::fd_limit(sched::current_pid()) { return -22; } // EINVAL
            let mut ep = EPOLL_INSTANCES.lock();
            let mut t = EPOLL_FDS.lock();
            let slot = match t.slot_for(i, tgid) { Some(s) => s, None => return -9 };
            if !ep[slot].in_use { return -9; }
            match t.alloc(slot, tgid, cmd == F_DUPFD_CLOEXEC) {
                Some(n) => { ep[slot].refs += 1; (EPOLL_FD_BASE + n) as isize }
                None => -24, // EMFILE
            }
        }
        _ => if EPOLL_FDS.lock().held(i, tgid).is_some() { 0 } else { -9 }, // F_GETFL/F_SETFL
    }
}

/// A new epoll handle for thread group `dst` on the instance that handle `h`
/// names in thread group `src` (pidfd_getfd). EBADF unless `src` holds `h`.
/// The caller gives the new handle a descriptor (or closes it).
fn epoll_dup_into(h: usize, src: u32, dst: u32, cloexec: bool) -> isize {
    let i = match epoll_fd_index(h) { Some(i) => i, None => return -9 };
    let mut ep = EPOLL_INSTANCES.lock();
    let mut t = EPOLL_FDS.lock();
    let slot = match t.slot_for(i, src) { Some(s) => s, None => return -9 };
    if !ep[slot].in_use { return -9; }
    match t.alloc(slot, dst, cloexec) {
        Some(n) => { ep[slot].refs += 1; (EPOLL_FD_BASE + n) as isize }
        None => -24, // EMFILE
    }
}

/// execve: close the calling process's close-on-exec epoll fds (the VFS and
/// the net server sweep their own tables). Epoll fds are a global table keyed
/// by holder, so without this every EPOLL_CLOEXEC instance survived exec and
/// held its slot of the global pool until the process exited.
fn epoll_exec_cloexec(tgid: u32) {
    let fds: alloc::vec::Vec<usize> = EPOLL_FDS.lock().held_by(tgid).into_iter()
        .filter(|&(_, c)| c).map(|(i, _)| i).collect();
    for i in fds { let _ = epoll_fd_drop(i, tgid); }
}

/// First epoll handle (see `vfs::EPOLL_HANDLE_BASE`): above every descriptor
/// number and every socket handle. Not a descriptor itself since lane fdmax.
const EPOLL_FD_BASE: usize = vfs::EPOLL_HANDLE_BASE;
/// One past the last handle of any kind.
const HANDLE_SPACE_END: usize = vfs::HANDLE_SPACE_END;

/// Maximum epoll fds traversed while computing readiness for a nested epoll
/// (Linux's `EP_MAX_NESTS`). Bounds the recursion in `poll_fd_state_nested`,
/// which is otherwise unbounded if an instance transitively watches itself —
/// nothing stops `epoll_ctl(ep, ADD, ep)` here. At the limit the fd reports
/// not-ready, which can delay an event but can never busy-spin.
const EPOLL_MAX_NEST: u32 = 4;

/// epoll_event.events flag bits (high bits, above the POLL* revent bits).
const EPOLLET:       u32 = 0x8000_0000; // edge-triggered
const EPOLLONESHOT:  u32 = 0x4000_0000; // fire once, then disarm until MOD
const EPOLLEXCLUSIVE: u32 = 1 << 28;    // wake-one across instances (see `sched::PollSet::add_excl`)
/// The bits EPOLLEXCLUSIVE may come with (Linux `EPOLLEXCLUSIVE_OK_BITS`):
/// IN, OUT, ERR, HUP, WAKEUP, ET and itself.
const EPOLLEXCLUSIVE_OK: u32 = 0x001 | 0x004 | 0x008 | 0x010 | (1 << 29) | EPOLLET | EPOLLEXCLUSIVE;

/// The instance slots that instance `s` watches (its epoll-fd registrations).
fn epoll_children(ep: &[EpollInstance], s: usize, out: &mut alloc::vec::Vec<usize>) {
    out.clear();
    let Some(inst) = ep.get(s) else { return };
    if !inst.in_use { return; }
    for it in inst.interests[..(inst.hi as usize).min(inst.interests.len())].iter() {
        if it.in_use && is_epoll_handle(it.fd as usize) {
            if let Some(c) = epoll_slot_of(it.fd as usize) { out.push(c); }
        }
    }
}

/// Longest chain of links below `s` (Linux `ep_loop_check_proc`); more than
/// `EPOLL_MAX_NEST` when it reaches `into` (a cycle) or runs too deep.
fn epoll_depth_down(ep: &[EpollInstance], s: usize, into: usize, depth: u32) -> u32 {
    let mut kids = alloc::vec::Vec::new();
    epoll_children(ep, s, &mut kids);
    let mut r = 0;
    for c in kids {
        if c == into || depth > EPOLL_MAX_NEST { return EPOLL_MAX_NEST + 1; }
        r = r.max(epoll_depth_down(ep, c, into, depth + 1) + 1);
        if r > EPOLL_MAX_NEST { return r; }
    }
    r
}

/// Longest chain of links above `s` (Linux `ep_get_upwards_depth_proc`).
fn epoll_depth_up(ep: &[EpollInstance], s: usize, depth: u32) -> u32 {
    if depth > EPOLL_MAX_NEST { return EPOLL_MAX_NEST + 1; }
    let mut kids = alloc::vec::Vec::new();
    let mut r = 0;
    for p in 0..ep.len() {
        if !ep[p].in_use || p == s { continue; }
        epoll_children(ep, p, &mut kids);
        if kids.contains(&s) {
            r = r.max(epoll_depth_up(ep, p, depth + 1) + 1);
            if r > EPOLL_MAX_NEST { return r; }
        }
    }
    r
}

/// Would `into` watching `to` close a loop or nest deeper than Linux allows
/// (`ep_loop_check`: down from `to`, plus the new link, plus up from
/// `into`, at most EP_MAX_NESTS = 4 links)? Called under EPOLL_INSTANCES.
fn epoll_loop_check(ep: &[EpollInstance], into: usize, to: usize) -> bool {
    if into == to { return true; }
    let down = epoll_depth_down(ep, to, into, 0);
    if down > EPOLL_MAX_NEST { return true; }
    down + 1 + epoll_depth_up(ep, into, 0) > EPOLL_MAX_NEST
}

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

static EPOLL_INSTANCES: sched::lockwatch::TrackedMutex<alloc::vec::Vec<EpollInstance>, { sched::lockwatch::L_EPOLL }> =
    sched::lockwatch::TrackedMutex::new_typed(alloc::vec::Vec::new());

/// Close an epoll fd alias: drop its fd entry; release the instance slot
/// (and all interests with it) when the last alias goes away.
fn sys_epoll_close(epfd: usize) -> isize {
    match epoll_fd_index(epfd) {
        Some(i) => epoll_fd_drop(i, sched::current_tgid()),
        None => -9,
    }
}

/// Drop `tgid`'s copy of epoll fd index `i`; the instance goes with the
/// last reference to it (every number aliasing it, in every process).
fn epoll_fd_drop(i: usize, tgid: u32) -> isize {
    let mut ep = EPOLL_INSTANCES.lock();
    let mut t = EPOLL_FDS.lock();
    match t.drop_holder(i, tgid) {
        None => -9, // EBADF
        Some(usize::MAX) => {
            let slot = t.t[i].slot as usize;
            if ep[slot].owner_tgid == tgid { epoll_rehome(&mut ep[slot], slot, &t, tgid); }
            0
        }
        Some(slot) => {
            ep[slot].refs = ep[slot].refs.saturating_sub(1);
            if ep[slot].refs == 0 { ep[slot] = EpollInstance::empty(); }
            else if ep[slot].owner_tgid == tgid { epoll_rehome(&mut ep[slot], slot, &t, tgid); }
            0
        }
    }
}

/// The creator `gone` of a still-referenced instance no longer holds any
/// number on it: name a surviving holder as the owner, whose fd table the
/// number-keyed (ofd 0) registrations and the holder search start from.
fn epoll_rehome(inst: &mut EpollInstance, slot: usize, t: &EpollFdTable, gone: u32) {
    let still = t.t.iter().enumerate().any(|(i, e)| e.in_use && e.slot as usize == slot && t.held(i, gone).is_some());
    if still { return; }
    if let Some(h) = t.any_holder_of(slot) { inst.owner_tgid = h; inst.owner_pid = h; }
}

/// Process exit: drop every epoll fd thread group `tgid` holds. An instance
/// is freed with its last reference — a forked child (or the parent) still
/// holding a copy keeps it alive. Without this a process that dies without
/// closing its epoll fds would leak instance slots (there are only
/// MAX_EPOLL_INSTANCES of them for the whole system).
fn epoll_close_all(tgid: u32) {
    let held = { EPOLL_FDS.lock().held_by(tgid) };
    for (i, _) in held { let _ = epoll_fd_drop(i, tgid); }
}

/// One serial line per table, the first time a global epoll table runs dry.
fn report_epoll_full(what: &str, cap: usize) {
    static REPORTED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let bit = if what == "fd" { 1 } else { 2 };
    if REPORTED.fetch_or(bit, core::sync::atomic::Ordering::Relaxed) & bit != 0 { return; }
    serial_print_str("\n[EPOLL] global ");
    serial_print_str(what);
    serial_print_str(" table FULL (ENFILE) at ");
    crate::print_number(cap as u32);
    serial_print_str("\n");
}

/// Ctrl-T dump hook: the GLOBAL epoll pools (instances, fds). Both are
/// session-wide fixed tables, so their fill level is what predicts the next
/// "EMFILE in a fresh process". IRQ context: try_lock only.
fn dump_epoll_census() {
    extern "C" { fn arch_serial_putc(c: u8); }
    fn ps(s: &str) { for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } } }
    fn pn(mut v: usize) {
        let mut buf = [0u8; 20]; let mut i = buf.len();
        loop { i -= 1; buf[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
        for &b in &buf[i..] { unsafe { arch_serial_putc(b); } }
    }
    ps("[EPOLL]");
    match EPOLL_INSTANCES.try_lock() {
        Some(ep) => { ps(" instances="); pn(ep.iter().filter(|e| e.in_use).count()); ps("/"); pn(MAX_EPOLL_INSTANCES); }
        None => ps(" instances=busy"),
    }
    match EPOLL_FDS.try_lock() {
        Some(t) => {
            ps(" fds="); pn(t.t.iter().filter(|e| e.in_use).count()); ps("/"); pn(MAX_EPOLL_FDS);
            ps(" forkcopies="); pn(t.extra.len());
        }
        None => ps(" fds=busy"),
    }
    // Open file descriptions (vfs::ofd): a count that only grows across
    // open/close churn is a leaked reference.
    match vfs::ofd::census() {
        Some((live, cap)) => { ps(" ofds="); pn(live); ps("/"); pn(cap); }
        None => ps(" ofds=busy"),
    }
    ps("\n");
    // Diagnostic (termsegv): every live epoll instance, its fd aliases and
    // its interests (fd:events:armed), so a waiter that never wakes can be
    // checked for a missing registration vs a lost wake.
    if EPOLL_DUMP_INTERESTS {
        let mut got = None;
        for _ in 0..100_000 {
            if let Some(ep) = EPOLL_INSTANCES.try_lock() {
                if let Some(t) = EPOLL_FDS.try_lock() { got = Some((ep, t)); break; }
            }
            core::hint::spin_loop();
        }
        if got.is_none() { ps("[EPI] busy\n"); }
        if let Some((ep, t)) = got {
            for (i, inst) in ep.iter().enumerate() {
                if !inst.in_use { continue; }
                ps("[EPI] s="); pn(i); ps(" tgid="); pn(inst.owner_tgid as usize); ps(" epfd=");
                for (j, e) in t.t.iter().enumerate() { if e.in_use && e.slot as usize == i { pn(EPOLL_FD_BASE + j); ps(","); } }
                ps(" :");
                for it in inst.interests[..inst.hi as usize].iter() {
                    if !it.in_use { continue; }
                    ps(" "); pn(it.fd as usize); ps("/"); pn(it.events as usize);
                    if it.ofd != 0 { ps("@"); pn(it.ofd as usize); }
                    if it.ptgid != 0 { ps("^"); pn(it.ptgid as usize); ps(":"); pn(it.pfd as usize); }
                    if !it.armed { ps("/off"); }
                }
                ps("\n");
            }
        }
    }
    let mut per = [(0u32, 0u32); 16];
    if let Some((n, objs)) = drivers::drm_device_interface::blob_census(&mut per) {
        ps("[DRMH] blob_objs="); pn(objs);
        for &(o, c) in &per[..n] { ps(" open"); pn(o as usize); ps("="); pn(c as usize); }
        ps("\n");
    }
    if FD_PROVENANCE_BT { dump_prime_notes(); }
}

/// PRIME imports (diagnostic, Ctrl-T, `FD_PROVENANCE_BT` only): the user
/// backtrace of the most recent PRIME_FD_TO_HANDLE that returned each handle,
/// so a gem handle an importer never closes can be traced to its importer.
const PRIME_NOTES: usize = 256;
static PRIME_NOTE: spin::Mutex<[(u32, u32, u32, [u64; 16]); PRIME_NOTES]> = spin::Mutex::new([(0, 0, 0, [0; 16]); PRIME_NOTES]);
static PRIME_SEQ: AtomicU32 = AtomicU32::new(0);

fn prime_import_note(open_id: u32, handle: u32, bt: [u64; 16]) {
    let seq = PRIME_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let mut t = PRIME_NOTE.lock();
    let slot = t.iter().position(|e| e.1 == handle && e.0 == open_id)
        .unwrap_or_else(|| (0..PRIME_NOTES).min_by_key(|&i| t[i].2).unwrap_or(0));
    t[slot] = (open_id, handle, seq, bt);
}

fn dump_prime_notes() {
    extern "C" { fn arch_serial_putc(c: u8); }
    fn ps(s: &str) { for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } } }
    fn px(v: u64) {
        const H: &[u8; 16] = b"0123456789abcdef";
        let mut i = 60i32; let mut st = false;
        while i >= 0 { let d = ((v >> i) & 0xF) as usize; if d != 0 || st || i == 0 { st = true; unsafe { arch_serial_putc(H[d]); } } i -= 4; }
    }
    let t = match PRIME_NOTE.try_lock() { Some(t) => t, None => return };
    for e in t.iter() {
        if e.2 == 0 { continue; }
        if drivers::drm_device_interface::gem_handle_live(e.1) != Some(true) { continue; }
        ps("[PRIME] open="); px(e.0 as u64); ps(" h="); px(e.1 as u64); ps(" q="); px(e.2 as u64); ps(" bt=");
        for b in e.3 { if b == 0 { break; } px(b); ps(","); }
        ps("\n");
    }
}

fn sys_epoll_create1(flags: usize) -> isize {
    const EPOLL_CLOEXEC: usize = 0x8_0000;
    {
        static HOOKED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        if !HOOKED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            sched::register_dump_hook(dump_epoll_census);
            vfs::ofd::set_release_hook(epoll_ofd_released);
        }
    }
    // Owner is the thread group, not the creating thread: the instance must
    // survive its creator thread's exit and be cleaned up with the process.
    let pid = sched::current_tgid();
    let mut ep = EPOLL_INSTANCES.lock();
    let mut t = EPOLL_FDS.lock();
    let i = match ep.iter().position(|e| !e.in_use) {
        Some(i) => i,
        None if ep.len() < MAX_EPOLL_INSTANCES && ep.try_reserve(1).is_ok() => {
            ep.push(EpollInstance::empty());
            ep.len() - 1
        }
        None => { drop(t); drop(ep); report_epoll_full("instance", MAX_EPOLL_INSTANCES); return -23; } // ENFILE: global table
    };
    match t.alloc(i, pid, flags & EPOLL_CLOEXEC != 0) {
        Some(fd_idx) => {
            ep[i] = EpollInstance::empty();
            ep[i].in_use    = true;
            ep[i].owner_pid  = pid;
            ep[i].owner_tgid = pid;
            ep[i].refs      = 1;
            ep[i].id        = epoll_next_seq();
            drop(t); drop(ep);
            publish_handle(current_pid(), fd_idx + EPOLL_FD_BASE, 0)
        }
        None => { drop(t); drop(ep); report_epoll_full("fd", MAX_EPOLL_FDS); -23 } // ENFILE: global table
    }
}

/// sys_epoll_ctl(epfd, op, fd, event_ptr)
fn sys_epoll_ctl(epfd: usize, op: usize, fd: usize, event_ptr: usize) -> isize {
    // EPOLL_CTL_ADD=1, EPOLL_CTL_DEL=2, EPOLL_CTL_MOD=3
    const CTL_ADD: usize = 1;
    const CTL_DEL: usize = 2;
    const CTL_MOD: usize = 3;

    // Ownership is thread-group-scoped, not task-scoped: an epoll instance is
    // an ordinary fd, shared by every CLONE_THREAD sibling of the creating
    // process (real POSIX fd-table semantics) — see the identical comment on
    // sys_epoll_wait's check for the bug this fixes — and by a fork child's
    // copy of the number (`epoll_fork_dup`).
    let tgid = sched::current_tgid();
    // The event is read before EPOLL_INSTANCES is taken, fault-tolerantly: a
    // fault under that lock stalls every epoll caller, and a bad pointer is
    // EFAULT, not a killed caller holding it. (Not necessarily 8-byte
    // aligned on x86_64 — the data word sits at offset 4 there.)
    let ev = if op != CTL_DEL {
        if event_ptr == 0 || !validate_user_buf(event_ptr, EPOLL_EVENT_SIZE) { return -14; }
        match (sched::uaccess::read_user::<u32>(event_ptr),
               sched::uaccess::read_user::<u64>(event_ptr + EPOLL_EVENT_DATA_OFF)) {
            (Some(e), Some(d)) => (e, d),
            _ => return -14,
        }
    } else { (0, 0) };
    // Linux's `do_epoll_ctl` order: EBADF for either fd, EPERM for a target
    // that cannot be polled, EINVAL for an epfd that is not an epoll fd or a
    // target that is the instance itself, then the op's own EEXIST/ENOENT.
    let ep_slot = epoll_slot_for(epfd, tgid);
    if ep_slot.is_none() && (epoll_fd_index(epfd).is_some() || fd_open_ofd(tgid, epfd).is_none()) {
        return -9; // EBADF
    }
    // The description `fd` names now: half of the item's key (see
    // `EpollInterest::ofd`). Resolved before EPOLL_INSTANCES (it takes the
    // fd-table locks).
    let ofd = match fd_open_ofd(tgid, fd) { Some(o) => o, None => return epoll_ctl_err(tgid, op, fd, -9) };
    // Regular files, directories and block devices have no poll on Linux
    // (`file_can_poll`), so epoll refuses them — callers (mio's SourceFd
    // users, tokio) take EPERM as "use blocking I/O". Checked up front for
    // ADD only: such an fd is never registered, so MOD/DEL (calloop's hot
    // re-arm path) pay for it only when they find no item (below).
    if op == CTL_ADD && fd < net_server::SOCK_FD_BASE && vfs::fd_epoll_eperm(tgid, fd) {
        return epoll_ctl_err(tgid, op, fd, -1);
    }
    let slot = match ep_slot { Some(s) => s, None => return -22 }; // EINVAL: not an epoll fd
    if epoll_slot_for(fd, tgid) == Some(slot) { return -22; } // EINVAL: the instance itself
    // EPOLLEXCLUSIVE (Linux `do_epoll_ctl`): not on MOD, not on an epoll
    // target, and only with EPOLLIN/OUT/ERR/HUP/WAKEUP/ET beside it.
    let target_ep = is_epoll_handle(fd);
    if op != CTL_DEL && ev.0 & EPOLLEXCLUSIVE != 0 {
        if op == CTL_MOD { return -22; }
        if op == CTL_ADD && (target_ep || ev.0 & !EPOLLEXCLUSIVE_OK != 0) { return -22; }
    }
    let mut ep = EPOLL_INSTANCES.lock();
    if !ep[slot].in_use { return -9; }
    // ELOOP: an ADD of an epoll fd that would close a cycle of instances or
    // make a chain longer than EP_MAX_NESTS links (checked before EEXIST, as
    // Linux does).
    if op == CTL_ADD && target_ep {
        if let Some(to) = epoll_slot_of(fd) {
            if epoll_loop_check(&ep[..], slot, to) { drop(ep); return -40; }
        }
    }

    let inst = &mut ep[slot];
    let cur = inst.interests[..inst.hi as usize].iter().position(|i| i.is(fd as i32, ofd));
    let r = match (op, cur) {
        (CTL_ADD, Some(_)) => -17, // EEXIST
        (CTL_ADD, None) => match inst.interests.iter().position(|i| !i.in_use).or_else(|| {
            if inst.interests.len() >= MAX_EPOLL_INTERESTS || inst.interests.try_reserve(1).is_err() { return None; }
            inst.interests.push(EpollInterest::empty());
            Some(inst.interests.len() - 1)
        }) {
            Some(i) => {
                inst.interests[i] = EpollInterest { fd: fd as i32, events: ev.0, data: ev.1, in_use: true,
                    last_seq: u64::MAX, armed: true, ofd, ptgid: 0, pfd: 0, mseq: epoll_next_seq() };
                if i as u32 >= inst.hi { inst.hi = i as u32 + 1; }
                // Under EPOLL_INSTANCES, after the insert: a description
                // whose last reference went before this marking is gone
                // (the fd was closed under us) and the item goes with it;
                // one released after it runs the release hook, which waits
                // for this lock and then removes the item.
                if ofd != 0 && !vfs::ofd::mark_watched(ofd) {
                    inst.interests[i] = EpollInterest::empty();
                    -9
                } else { 0 }
            }
            None => -28, // ENOSPC: max_user_watches
        },
        // MOD re-arms too: it is how a caller re-arms an EPOLLONESHOT
        // interest that disarmed itself after firing. The holder cache
        // (ptgid/pfd) stays: the item still names the same description.
        // Linux leaves an EPOLLEXCLUSIVE registration alone on MOD: EINVAL.
        (CTL_MOD, Some(i)) if inst.interests[i].events & EPOLLEXCLUSIVE != 0 => -22,
        (CTL_MOD, Some(i)) => {
            let it = &mut inst.interests[i];
            it.events = ev.0; it.data = ev.1; it.last_seq = u64::MAX; it.armed = true;
            it.mseq = epoll_next_seq();
            0
        }
        (CTL_DEL, Some(i)) => { inst.interests[i] = EpollInterest::empty(); 0 }
        (CTL_MOD, None) | (CTL_DEL, None) => -2, // ENOENT
        _ => -22, // EINVAL
    };
    // Published while the mutation is already visible (the lock was held
    // above): a waiter that read the old generation before its probe pass
    // sees the new one after it parks and re-probes (see `epoll_wait_until`).
    if r >= 0 { EPOLL_CTL_GEN.fetch_add(1, Ordering::AcqRel); }
    drop(ep);
    // THE ONE REAL LOST-WAKE HOLE (see the targeted-wake design): a sibling
    // thread may be parked in epoll_wait on this instance with a `poll_mask`
    // computed BEFORE this add/mod, so a *targeted* wake for the newly armed
    // fd would never reach it. Every epoll_wait park on this instance, and
    // every mask an enclosing epoll/poll/select computes for it
    // (`epoll_tag_mask`), carries `poll_tag(EPOLL, slot)`.
    //
    // Linux wakes those waiters only when the (re-)armed item is ready now
    // (ep_insert/ep_modify: `if (revents && !ep_is_linked(...)) wake_up`).
    // This kernel used to wake them on EVERY successful mutation, and
    // calloop/`polling` re-arm an EPOLLONESHOT interest with MOD after every
    // event it dispatches: on the idle greeter that was ~80 epoll_ctl calls
    // a second, each waking every thread whose mask shared the instance's
    // tag bit to re-probe and park again (~55 spurious wakes/s in
    // cosmic-comp + cosmic-greeter). Now:
    //  - ready now → wake the instance's waiters (Linux);
    //  - not ready → no wake; instead OR the fd's tag into every parked mask
    //    that carries the instance's tag, so the fd's own later edge reaches
    //    them (what the wake used to buy);
    //  - DEL → nothing (Linux does not wake on removal either).
    // A waiter between its probe pass and its park is covered by
    // EPOLL_CTL_GEN, which it re-checks after publishing Blocked.
    if r >= 0 && (op == CTL_ADD || op == CTL_MOD) {
        let (cur, _seq, tag) = probe_fd_events_seq(current_pid(), fd, ev.0);
        // Keyed (lane epnameless): every filter that watches this instance
        // names its key (`PollSet::add_key`), so neither the wake nor the
        // widening reaches a parker that merely shares the tag bit. The
        // bit-only versions widened unrelated parkers' masks and let the new
        // fd's wakes through to them (desktop: ~60 % of useless epoll wakes,
        // a ring naming bits no registration has).
        let self_key = epoll_self_key(slot);
        if cur != 0 {
            sched::wake_poll_key(self_key);
        } else {
            sched::widen_poll_masks(self_key, tag.bits);
        }
    }
    if r == -2 && fd < net_server::SOCK_FD_BASE && vfs::fd_epoll_eperm(tgid, fd) {
        return epoll_ctl_err(tgid, op, fd, -1); // EPERM, as Linux checks before the lookup
    }
    if r == -17 || r == -2 { return epoll_ctl_err(tgid, op, fd, r); }
    r
}

/// Serial note for an epoll_ctl refused with EEXIST/ENOENT/EPERM/EBADF (the
/// errors this kernel used to swallow: ADD acted as MOD, DEL/MOD of a missing
/// item and ADD of a closed fd or a regular file all returned 0). The first
/// `EPOLL_CTL_ERR_LOG` of them, with the caller's executable, so a client that
/// depended on the old lenience shows up in a session log.
const EPOLL_CTL_ERR_LOG: u32 = 32;
fn epoll_ctl_err(tgid: u32, op: usize, fd: usize, r: isize) -> isize {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    if SEEN.fetch_add(1, Ordering::Relaxed) < EPOLL_CTL_ERR_LOG {
        let mut name = [0u8; 64];
        let n = sched::exe_path(tgid, &mut name).unwrap_or(0);
        serial_print_str("[EPOLL] ctl err=");
        crate::print_number((-r) as u32);
        serial_print_str(" op=");
        crate::print_number(op as u32);
        serial_print_str(" fd=");
        crate::print_number(fd as u32);
        serial_print_str(" tgid=");
        crate::print_number(tgid);
        serial_print_str(" ");
        serial_print_str(core::str::from_utf8(&name[..n]).unwrap_or("?"));
        serial_print_str("\n");
    }
    r
}

/// Bumped by every successful epoll_ctl. A poll/epoll waiter reads it before
/// its probe pass and again after publishing Blocked: a change means an
/// interest set it may be watching (directly or nested) changed after it
/// computed its mask, so it re-probes instead of parking on a stale mask. A
/// ctl that lands after the second read finds the waiter already Blocked and
/// widens its mask (`sched::widen_poll_masks`).
static EPOLL_CTL_GEN: AtomicU64 = AtomicU64::new(0);

/// The generation a poll()/select() pass checks after publishing Blocked:
/// EPOLL_CTL_GEN plus `sched::poll_retag_gen` (a socket whose poll tag
/// changed with its state, e.g. an unbound UDP socket bound by another
/// thread: a mask collected before the change names only keys that were
/// already woken). Both only grow, so the sum moves iff either does.
fn epoll_ctl_gen() -> u64 { EPOLL_CTL_GEN.load(Ordering::Acquire) }

fn poll_gen() -> u64 {
    EPOLL_CTL_GEN.load(Ordering::Acquire).wrapping_add(sched::poll_retag_gen())
}

/// sys_epoll_wait(epfd, events_ptr, maxevents, timeout_ms)
///
/// `timeout_ms` follows `epoll_wait(2)`: `usize::MAX` (the bit pattern of a
/// raw `-1` `c_int` sign-extended by relibc's `syscall!` macro) blocks
/// indefinitely, `0` checks once and returns immediately, otherwise it's a
/// millisecond budget rounded to this kernel's ~10ms tick granularity.
/// Returns the number of ready events, or 0 on timeout.
fn sys_epoll_wait(epfd: usize, events_ptr: usize, maxevents: usize, timeout: usize) -> isize {
    // The low 32 bits are the C `int` timeout in ms; -1 = forever (whether
    // the caller sign- or zero-extended it into the register).
    let timeout_ms = timeout as i32;
    let (infinite, deadline) = if timeout_ms < 0 { (true, 0) }
        else { (false, deadline_after_ms(timeout_ms as u64)) };
    epoll_wait_until(epfd, events_ptr, maxevents, infinite, deadline)
}

/// sys_epoll_pwait(epfd, events_ptr, maxevents, timeout_ms, sigmask, sigsetsize)
///
/// `epoll_wait` under a temporary signal mask (NULL = keep the caller's),
/// installed and restored atomically around the wait exactly as ppoll/pselect6
/// do (`masked_timed_wait`): a signal the temporary mask unblocks ends the wait
/// with EINTR and its handler runs under that mask; any other result restores
/// the caller's mask at once (Linux `restore_saved_sigmask_unless(-EINTR)`).
/// aarch64 has no plain epoll_wait, so every epoll wait there comes through
/// here; the mask used to be ignored.
fn sys_epoll_pwait(epfd: usize, events_ptr: usize, maxevents: usize, timeout: usize,
                   sigmask_ptr: usize, sigsetsize: usize) -> isize {
    let mask = match epoll_sigmask(sigmask_ptr, sigsetsize) { Ok(m) => m, Err(e) => return e };
    masked_timed_wait(mask, 0, 0, || sys_epoll_wait(epfd, events_ptr, maxevents, timeout))
}

/// The sigmask argument of epoll_pwait/epoll_pwait2: NO_SIGMASK when NULL
/// (sigsetsize is then not checked, as on Linux).
fn epoll_sigmask(sigmask_ptr: usize, sigsetsize: usize) -> Result<u64, isize> {
    if sigmask_ptr == 0 { Ok(NO_SIGMASK) } else { read_user_sigmask(sigmask_ptr, sigsetsize) }
}

/// sys_epoll_pwait2(epfd, events_ptr, maxevents, timeout_ptr, sigmask, sigsetsize)
///
/// The timeout is a `struct timespec *` (NULL = forever), NOT milliseconds:
/// routing this number to `sys_epoll_wait` read the pointer as a timeout of
/// ~10^14 ms, so every epoll_pwait2 with a finite timeout waited forever.
/// The sigmask is handled as in `sys_epoll_pwait`; the timeout is not written
/// back (Linux does not).
fn sys_epoll_pwait2(epfd: usize, events_ptr: usize, maxevents: usize, timeout_ptr: usize,
                    sigmask_ptr: usize, sigsetsize: usize) -> isize {
    let (infinite, deadline) = if timeout_ptr == 0 { (true, 0) } else {
        match read_user_timespec(timeout_ptr) {
            Ok(ns) => (false, deadline_after_ns(ns)),
            Err(e) => return e,
        }
    };
    let mask = match epoll_sigmask(sigmask_ptr, sigsetsize) { Ok(m) => m, Err(e) => return e };
    masked_timed_wait(mask, 0, 0, || epoll_wait_until(epfd, events_ptr, maxevents, infinite, deadline))
}

/// Shared body: wait until an interest is ready, `deadline` (absolute
/// `monotonic_ns`, ignored when `infinite`) passes, or a signal arrives.
fn epoll_wait_until(epfd: usize, events_ptr: usize, maxevents: usize, infinite: bool, deadline: u64) -> isize {
    if maxevents == 0 { return -22; }
    if !validate_user_buf(events_ptr, maxevents * EPOLL_EVENT_SIZE) { return -14; }

    // Held by the caller's thread group: its creator, a dup, or a fork copy.
    let slot = match epoll_slot_for(epfd, sched::current_tgid()) {
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
        if !ep[slot].in_use { return -9; }
    }

    // EPOLLET (edge-triggered) fires only when the object's per-event seq
    // advanced since the last delivery — so a permanently-level-ready fd (a
    // pipe at EOF, mio's never-drained eventfd waker, a level-writable socket)
    // can't storm a tokio reactor that registered EPOLLET. Level interests
    // (calloop's default) fire whenever a ready bit is set. A fd with no edge
    // source (net listener, fd 0-2) reports seq None and stays level even
    // under EPOLLET. K2 additionally blocks on the global poll wait-channel
    // instead of yield-spinning; see §1 of the design.
    if gd_stats() {
        gd_inc(K_EP_CALL);
        let now = monotonic_ns();
        if !infinite && deadline <= now { gd_inc(K_EP_ZERO); }
        else if !infinite && deadline - now <= 2_000_000 { gd_inc(K_EP_SHORT); }
    }
    if EPOLL_READY.load(Ordering::Relaxed) {
        return epoll_wait_ready(pid, epfd, slot, events_ptr, maxevents, infinite, deadline);
    }
    let mut gd_woke = 0u64;
    loop {
        let gen0 = poll_gen();
        // ---- PROBE ---- per-interest snapshot: hold EPOLL_INSTANCES only to
        // copy one interest out, drop it before probe_fd_events_seq (which
        // calls vfs/net and must never run under a spinlock — invariant
        // 82d0cc3), write user memory lock-free, then re-lock briefly to
        // commit last_seq / disarm ONESHOT, matching by fd since a sibling
        // epoll_ctl may have mutated the slot meanwhile.
        let mut n = 0usize;
        // This instance's own tag (what epoll_ctl on it wakes) OR every armed
        // interest's targeted-wake tag; becomes this task's
        // `poll_mask` if we park below. The loop only `break`s early once an
        // event has fired (n > 0), in which case we return before using `mask`,
        // so whenever we reach the park `mask` has seen every armed interest.
        let mut set = sched::PollSet::new();
        set.add_key(epoll_self_key(slot)); // epoll_ctl wakes it by this key
        // Re-read the high-water mark on every pass: a CLONE_THREAD sibling
        // sharing this epoll fd may epoll_ctl(ADD) while we are blocked below,
        // and the new interest must be visible to the next probe.
        let hi = { EPOLL_INSTANCES.lock()[slot].hi as usize };
        for i in 0..hi {
            if n >= maxevents { break; }
            let interest = { EPOLL_INSTANCES.lock()[slot].interests[i] };
            if !interest.in_use || !interest.armed { continue; }
            let (cur, seq, tag) = probe_interest(pid, slot, &interest, 0);
            if CHURN_STATS && tag.bits == sched::POLL_TAG_ALL { churn_all_tag_note(pid, interest.fd as usize); }
            poll_set_add(&mut set, pid, &tag, 0);
            let et = interest.events & EPOLLET != 0;
            let fire = cur != 0 && (!et || match seq {
                Some(s) => s != interest.last_seq,
                None    => true,
            });
            // POLLNVAL: the number is not open (any more). Linux has no
            // EPOLLNVAL — a released file's registration is gone — so a
            // registration that outlived its fd (a close path that does not
            // go through `epoll_release_fd`, e.g. exec's close-on-exec
            // sweep, or a close racing this snapshot) reports nothing. It is
            // left in place, not removed: a probe can also miss a live fd
            // transiently, and a removed registration never comes back.
            if cur & POLLNVAL_EP != 0 { continue; }
            if fire {
                let off = n * EPOLL_EVENT_SIZE;
                // See the read_unaligned note in sys_epoll_ctl (the
                // uaccess copy is bytewise-safe at any alignment).
                if !ua::write_user::<u32>(events_ptr + off, cur)
                    || !ua::write_user::<u64>(events_ptr + off + EPOLL_EVENT_DATA_OFF, interest.data) {
                    return -14;
                }
                if gd_stats() { gd_fd(interest.fd as usize, 0); }
                let mut ep = EPOLL_INSTANCES.lock();
                if let Some(j) = ep[slot].interests.iter()
                    .position(|x| x.is(interest.fd, interest.ofd)) {
                    if let Some(s) = seq { ep[slot].interests[j].last_seq = s; }
                    if interest.events & EPOLLONESHOT != 0 { ep[slot].interests[j].armed = false; }
                }
                n += 1;
            }
        }
        if gd_stats() {
            let timed_out = !infinite && monotonic_ns() >= deadline;
            gd_site(core::mem::take(&mut gd_woke), n > 0 || timed_out || interrupted(), false);
            if n > 0 { gd_inc(K_EP_EV); } else if timed_out { gd_inc(K_EP_TMO); }
            else if interrupted() { gd_inc(K_EP_INTR); }
        }
        if n > 0 { return n as isize; }
        if !infinite && monotonic_ns() >= deadline { return 0; }
        if interrupted() { return -4; } // EINTR — lets e.g. tokio's SIGCHLD handler run

        // ---- BLOCK ---- three-phase on the global poll wait-channel; the
        // re-probe between prepare and commit closes the check-then-sleep
        // lost-wake (an edge landing after the probe above lands as a
        // wake_poll against an already-Blocked task, or shows in the re-probe).
        // A zero mask (e.g. the only interest is an empty nested epoll) must
        // NOT park narrow — poll_mask 0 is immune even to a broadcast wake, a
        // hang. Fall back to broadcast.
        // (`block_on_poll_prepare_set` parks an empty set broadcast.)
        if gd_stats() && (set.mask() == 0 || set.mask() == sched::POLL_TAG_ALL) { gd_inc(K_PARKALL); }
        sched::block_on_poll_prepare_set(if infinite { u64::MAX } else { deadline }, &set);
        if poll_gen() != gen0
            || epoll_any_ready(pid, slot) || interrupted()
            || (!infinite && monotonic_ns() >= deadline) {
            sched::block_on_poll_cancel();
            continue;
        }
        if gd_stats() { gd_inc(K_EP_PARK); }
        sched::sysacct::note(sched::current_tgid(), sched::sysacct::NR_EPOLL_PARK);
        sched::block_on_poll_commit();
        if gd_stats() { gd_woke = sched::gdwake::take(pid); }
    }
}

// ── epoll ready list (lane epollready) ──────────────────────────────────────
//
// The probe-everything loop above re-probed every interest after every wake,
// twice per park (the pass, then `epoll_any_ready` between prepare and
// commit): ~2N VFS_POLL / NET_POLL round trips per useless wake, and N per
// call. On the laptop desktop that was most of cosmic-comp's, busd's and the
// panel's system time.
//
// Now, like Linux's ready list, a wake re-probes only what it can concern:
//
// - The calling thread is a poll-wake LISTENER (`sched::PollListen`) from its
//   first epoll_wait on an instance until it waits on something else: the
//   wake scans record every wake that passes its interest filter in a per-task
//   ring — exact keys, or the bits of bit-only wakes — whether it is parked,
//   running or in user space.
// - `EpCache` (per thread, in `Task::ep_cache`) remembers each registration's
//   last probe answer: its tag (bits, exact keys, foreign/nested), and whether
//   it was ready. A recorded key selects the registrations whose probe carried
//   that key, plus those holding the key's bit WITHOUT a key (an unkeyed
//   answer cannot be ruled out); recorded bits select the registrations whose
//   bits intersect them. Broadcast-tag and nested-epoll registrations are
//   re-probed on every pass.
// - The ready list: registrations that were ready at the last probe (a
//   level-triggered fd that stays ready, an edge-triggered one whose level is
//   up) are re-probed at the start of every call, exactly as Linux re-checks
//   its ready-list entries; new or epoll_ctl(MOD)ified registrations (their
//   `mseq` changed) are re-probed too.
// - Unknown reasons fall back to a full re-probe: a sleep that ended with no
//   recorded wake (signals, stop/continue), a broadcast wake (`tags == ALL`),
//   a stale snapshot (another instance, a new listener).
//
// Lost-wake safety: a wake that lands between a probe and the park is in the
// ring (the listener filter covers every registration probed so far), and
// `poll_listen_park` parks only if the ring is empty, under RUN_QUEUE. Before
// the first filter exists the listener records everything (filter ALL). A
// registration whose tag changed after the filter was built is re-probed once
// more after the new filter is installed. epoll_ctl on the instance bumps
// EPOLL_CTL_GEN, checked after the park is published (as before).

/// `epoll.ready=0` on the kernel command line: the probe-everything loop.
pub static EPOLL_READY: AtomicBool = AtomicBool::new(true);

/// `epoll.kids=0` on the kernel command line: nested registrations are not
/// given kid snapshots (`EpKids`) and are probed whole on every pass.
pub static EPOLL_KIDS: AtomicBool = AtomicBool::new(true);

/// One registration in a thread's epoll snapshot (`EpCache::ents[i]` mirrors
/// `EpollInstance::interests[i]`).
#[derive(Clone)]
struct EpEnt {
    used:  bool,
    fd:    i32,
    ofd:   u32,
    mseq:  u64,
    armed: bool,
    /// `tag` is a probe answer (else: never probed — always re-probed).
    known: bool,
    /// Ready (or not yet probed) at the last probe: re-probed at the next call.
    ready: bool,
    /// `tag` is in the listener's current filter (it was armed and known
    /// when the filter was last built).
    filt:  bool,
    tag:   sched::PollTag,
    /// A nested epoll registration's own snapshot of the inner instance (lane
    /// epollcost), once its first probe showed it is one: see `EpKids`.
    kids:  Option<alloc::boxed::Box<EpKids>>,
    /// Registered with EPOLLEXCLUSIVE: its keys are wake-one in the
    /// listener's filter (`sched::PollSet::add_excl`).
    excl:  bool,
}

impl EpEnt {
    const NONE: EpEnt = EpEnt { used: false, fd: 0, ofd: 0, mseq: 0, armed: false, known: false,
                                ready: false, filt: false, tag: sched::PollTag::NONE, kids: None, excl: false };
}

/// The inner instance of a nested epoll registration, snapshotted per
/// registration like the outer one (lane epollcost).
///
/// A nested registration used to be re-probed on every pass of every call,
/// and probing it asked EVERY registration of the inner instance twice (is
/// any ready, then the OR of their tags), plus once more for every filter
/// build. cosmic-comp's calloop instance nests wayland-server's (one AF_UNIX
/// end per client) and libinput's: ~40 inner probes, each a VFS/NET round
/// trip, per epoll_wait — 95 % of its epoll kernel time in
/// `epolltest costbench`, and most of its 368 µs per call on the laptop.
///
/// Now the inner registrations (`ents`, mirroring the inner instance's
/// interests) are judged like the outer ones: diffed against the inner
/// instance on every pass (new or MODified ones are probed), and otherwise
/// probed only when a recorded wake names them (`ep_hit` on their own tag),
/// when they fired at their last probe or their answer just changed
/// (`ready`), or when they have no per-object tag (broadcast, or nested once
/// more). The listener's filter holds each kid's own tag, keys included, so
/// a kid's wake is in the ring exactly as a direct registration's would be.
/// The nested registration fires when one of its kids does (Linux: an epoll
/// fd is readable while its ready list is non-empty). A kid's fire can only
/// turn on through a wake on its own tag (a readiness or edge change), a MOD
/// or an ADD (both seen by the diff) — the invariant the ready list rests on.
#[derive(Clone)]
struct EpKids {
    slot: usize,
    inst: u64,
    ents: alloc::vec::Vec<EpEnt>,
}

/// A thread's epoll snapshot of one instance, valid while its listener id is
/// `lid` and the instance in `slot` is `inst`.
struct EpCache {
    /// `EP_MAGIC` while the box is live, and the owner's
    /// `sched::current_slot_tag()` at creation (lane piepollsrc): a torn
    /// box (magic gone) and a box in a foreign slot are told apart.
    magic: u64,
    owner: (usize, u32),
    /// The per-call `cand`/`recheck`/`changed` vectors while the thread sleeps
    /// (a thread killed asleep is reaped without unwinding; see `ep_cache_put`).
    scratch: (alloc::vec::Vec<bool>, alloc::vec::Vec<u32>, alloc::vec::Vec<u32>),
    slot:  usize,
    inst:  u64,
    lid:   u32,
    ents:  alloc::vec::Vec<EpEnt>,
    /// Wakes taken from the ring but not yet acted on when the call returned.
    carry: sched::WakeRing,
    /// EPOLL_CTL_GEN as of the pass that last built the listener's filter.
    /// A nested registration's answer is keyless (its instance's bits), so
    /// a ctl on the inner instance that adds a key whose bit the filter
    /// already holds does not change it: the filter, which carries the
    /// inner keys `epoll_collect` found when it was built, would then miss
    /// the new key's wake (a hang: lane nowake). While this differs from the
    /// pass's generation, a nested answer counts as changed, so the filter is
    /// rebuilt and the nested entry rechecked.
    cgen:  u64,
    /// Snapshots of other instances this thread waited on, most recent last
    /// (at most `EP_OTHERS`). cosmic-comp's main thread alternates between
    /// calloop's instance and wayland-server's (a zero-timeout wait inside
    /// `dispatch_clients`, nested in the first), and with one snapshot per
    /// thread every switch was a cache miss: a full probe pass (lane nowake,
    /// 2109 of its 12029 calls in a desktop run). A kept snapshot stays valid
    /// because the listener's filter covers its registrations too (every
    /// filter build adds their `filt` entries) and every ring the listener
    /// hands over is also merged into each kept snapshot's `carry`; it is
    /// dropped whenever the listener restarts (another id).
    others: alloc::vec::Vec<EpSnap>,
}

/// A kept snapshot of another instance (`EpCache::others`).
struct EpSnap {
    slot:  usize,
    inst:  u64,
    ents:  alloc::vec::Vec<EpEnt>,
    carry: sched::WakeRing,
    cgen:  u64,
}

/// How many other instances' snapshots a thread keeps.
const EP_OTHERS: usize = 1;

/// A ring the listener handed over concerns the kept snapshots too.
fn ep_share(others: &mut [EpSnap], r: &sched::WakeRing) {
    for o in others.iter_mut() { ep_merge(&mut o.carry, r); }
}

/// The wake that ended a sleep is unknown: every kept snapshot re-probes all.
fn ep_share_unknown(others: &mut [EpSnap]) {
    for o in others.iter_mut() { o.carry.push_tags(sched::POLL_TAG_ALL); }
}

/// Why a pass re-probes.
enum EpWhy { Nothing, All, Ring(sched::WakeRing) }

/// The bits of a probe answer that no key of its own explains (`PollSet::add`'s
/// rule): a keyed wake on such a bit may be this object's.
fn ep_unkeyed_bits(t: &sched::PollTag) -> u64 {
    if t.foreign && t.n == 0 && t.nested == 0 && t.bits != sched::POLL_TAG_ALL { return 0; }
    let mut keyed = 0u64;
    for &k in &t.keys[..(t.n as usize).min(3)] {
        let b = sched::poll_key_bit(k);
        if t.bits & b != 0 { keyed |= b; }
    }
    t.bits & !keyed
}

/// Re-probed on every pass: no per-object identity to match a wake against.
fn ep_always(t: &sched::PollTag) -> bool { t.bits == sched::POLL_TAG_ALL || t.nested != 0 }

/// Can a wake in `r` concern the registration whose last answer was `t`?
fn ep_hit(t: &sched::PollTag, r: &sched::WakeRing) -> bool {
    if r.tags == sched::POLL_TAG_ALL || t.bits & r.tags != 0 { return true; }
    if r.n == 0 { return false; }
    let unk = ep_unkeyed_bits(t);
    let own = &t.keys[..(t.n as usize).min(3)];
    r.keys().iter().any(|&k| own.contains(&k) || unk & sched::poll_key_bit(k) != 0)
}

/// Classify a useless wake for `wakestat::EP_NAMELESS` (see there).
fn ep_nameless_why(woke_ring: bool, named: bool, r: &sched::WakeRing, self_tag: u64,
                   others: &[EpSnap], ents: &[EpEnt]) -> usize {
    if !woke_ring || r.tags == sched::POLL_TAG_ALL { return 1; }
    if named { return 0; }
    if r.n == 0 && r.tags == 0 { return if r.flags != 0 { 2 } else { 7 }; }
    if others.iter().any(|o| o.ents.iter().any(|e| e.used && e.known && ep_hit(&e.tag, r))) { return 3; }
    if r.tags & self_tag != 0 || r.keys().iter().any(|&k| sched::poll_key_bit(k) == self_tag) { return 4; }
    if ents.iter().any(|e| e.used && e.known && !e.armed && ep_hit(&e.tag, r)) { return 8; }
    if r.n > 0 { return 5; }
    6
}

fn ep_merge(a: &mut sched::WakeRing, b: &sched::WakeRing) {
    for &k in b.keys() { a.push_key(k); }
    a.push_tags(b.tags);
    a.flags |= b.flags;
}

/// Diff a snapshot's entries against `interests[..hi]` (the per-pass DIFF):
/// new or re-pointed registrations become unknown, MODified ones are marked
/// for a re-probe (`ready`, and `cand` when given) and, if the filter does
/// not hold them, unknown too. Returns whether anything needs a probe.
fn ep_diff(ents: &mut alloc::vec::Vec<EpEnt>, interests: &[EpollInterest], hi: usize,
           mut cand: Option<&mut alloc::vec::Vec<bool>>) -> bool {
    if ents.len() < hi { ents.resize(hi, EpEnt::NONE); }
    if let Some(c) = cand.as_deref_mut() { c.clear(); c.resize(ents.len(), false); }
    let mut any = false;
    for i in 0..ents.len() {
        let e = &mut ents[i];
        let it = if i < hi { interests[i] } else { EpollInterest::empty() };
        if !it.in_use { if e.used { *e = EpEnt::NONE; } continue; }
        if !e.used || e.fd != it.fd || e.ofd != it.ofd {
            *e = EpEnt { used: true, fd: it.fd, ofd: it.ofd, mseq: it.mseq, armed: it.armed,
                         known: false, ready: true, filt: false, tag: sched::PollTag::NONE, kids: None,
                         excl: it.events & EPOLLEXCLUSIVE != 0 };
            any = true;
        } else if e.mseq != it.mseq {
            // MOD: keep the old answer as a provisional tag while the
            // filter covers it (a changed one is rechecked). One the
            // filter does NOT cover — a ONESHOT entry that fired is
            // left out of every filter built while it is disarmed,
            // and a MOD re-arming it widens no listener's filter
            // (see `RunQueue::widen_port_masks`) — is re-probed as a
            // new registration, so the filter is rebuilt with it and
            // it is rechecked under that filter; else the re-armed
            // fd's wakes were never recorded (a hang).
            e.mseq = it.mseq; e.armed = it.armed; e.ready = true;
            e.excl = it.events & EPOLLEXCLUSIVE != 0;
            if !e.filt { e.known = false; }
            if let Some(c) = cand.as_deref_mut() { c[i] = true; }
            any = true;
        } else {
            e.armed = it.armed;
        }
    }
    any
}

/// Diff a nested registration's kids against its inner instance (`ep` held).
/// A slot that holds another instance now starts the kids over.
fn ep_diff_kids(k: &mut EpKids, ep: &[EpollInstance]) -> bool {
    let Some(inner) = ep.get(k.slot) else { k.ents.clear(); return true; };
    if inner.id != k.inst { k.ents.clear(); k.inst = inner.id; }
    let hi = if inner.in_use { (inner.hi as usize).min(inner.interests.len()) } else { 0 };
    ep_diff(&mut k.ents, &inner.interests, hi, None)
}

/// Must a nested registration's kids be looked at on this pass? (`ring`:
/// the recorded wakes, None = no recorded reason.) An epoll_ctl on the inner
/// instance wakes its own tag bit-only, so that bit counts as a hit too.
fn ep_kids_need(k: &EpKids, ring: Option<&sched::WakeRing>) -> bool {
    if let Some(r) = ring {
        if r.tags & epoll_self_tag(k.slot) != 0 || r.keys().contains(&epoll_self_key(k.slot)) { return true; }
    }
    k.ents.iter().any(|e| e.used && e.armed
        && (!e.known || e.ready || ep_always(&e.tag) || ring.map_or(false, |r| ep_hit(&e.tag, r))))
}

/// Add a snapshot entry's answer to a listener filter: its own tag, or for a
/// nested registration with kids its inner instance's tag plus each known
/// kid's own tag.
fn ep_filter_add(set: &mut sched::PollSet, pid: u32, e: &EpEnt) {
    match &e.kids {
        Some(k) => {
            set.add_key(epoll_self_key(k.slot)); // epoll_ctl wakes it by this key
            for x in k.ents.iter() {
                if x.used && x.armed && x.known { poll_set_add(set, pid, &x.tag, 1); }
            }
        }
        // An exclusive registration is never an epoll fd (EINVAL at ADD).
        None if e.excl && e.tag.nested == 0 => set.add_excl(&e.tag),
        None => poll_set_add(set, pid, &e.tag, 0),
    }
}

/// Probe a nested registration through its kids (see `EpKids`): the kids
/// that `full`, `ring` or their own state name. Returns (an armed kid fires,
/// a kid's answer changed). A kid whose answer changed stays `ready`, so the
/// recheck under the rebuilt filter probes it again.
fn ep_eval_kids(pid: u32, k: &mut EpKids, full: bool, ring: Option<&sched::WakeRing>,
                nested_stale: bool) -> (bool, bool) {
    use core::sync::atomic::Ordering::Relaxed;
    let (mut fire_any, mut changed) = (false, false);
    for j in 0..k.ents.len() {
        let x = &k.ents[j];
        if !x.used || !x.armed { continue; }
        if !(full || !x.known || x.ready || ep_always(&x.tag) || ring.map_or(false, |r| ep_hit(&x.tag, r))) {
            continue;
        }
        let it = {
            let ep = EPOLL_INSTANCES.lock();
            if ep[k.slot].id != k.inst || j >= ep[k.slot].interests.len() { break; } // the next diff
            ep[k.slot].interests[j]
        };
        let x = &mut k.ents[j];
        if !it.in_use || !it.armed || it.fd != x.fd || it.ofd != x.ofd { continue; } // the next diff
        sched::wakestat::EP_INNER.fetch_add(1, Relaxed);
        let (cur, seq, tag) = probe_interest(pid, k.slot, &it, 1);
        // A twice-nested kid's answer is keyless: a ctl below it may add a
        // key whose bit it already has (see `EpCache::cgen`).
        let ch = !x.known || x.tag != tag || (nested_stale && tag.nested != 0);
        if ch { x.tag = tag; x.known = true; changed = true; }
        let fire = cur & POLLNVAL_EP == 0 && cur != 0
            && (it.events & EPOLLET == 0 || match seq { Some(s) => s != it.last_seq, None => true });
        x.ready = fire || ch;
        fire_any |= fire;
    }
    (fire_any, changed)
}

/// A pass that ran out of `maxevents` before reaching a nested registration:
/// mark the kids it would have probed `ready`, so the next call probes them
/// although the wakes that named them are gone with this pass's ring.
fn ep_kids_defer(k: &mut EpKids, full: bool, ring: Option<&sched::WakeRing>) {
    let self_hit = ring.map_or(false, |r| r.tags & epoll_self_tag(k.slot) != 0);
    for x in k.ents.iter_mut() {
        if x.used && x.armed && (full || self_hit || ring.map_or(false, |r| ep_hit(&x.tag, r))) {
            x.ready = true;
        }
    }
}

/// The instance slot a nested registration's fd names now (as its probe
/// resolves it), or None when it is no epoll fd (any more).
fn ep_nested_slot(pid: u32, it: &EpollInterest) -> Option<usize> {
    if it.ofd != 0 || it.ptgid != 0 { return None; }
    let h = vfs::resolve_fd(pid, it.fd as usize);
    if is_epoll_handle(h) { epoll_slot_of(h) } else { None }
}

/// epoll_wait on the ready-list path (see above). Same contract as the loop
/// in `epoll_wait_until`.
/// Whether a kept epoll snapshot's vectors still look like vectors: a
/// buffer pointer in the kernel half (or none at all), len <= capacity.
fn ep_vec_sane<T>(v: &alloc::vec::Vec<T>) -> bool {
    let (p, len, cap) = (v.as_ptr() as usize, v.len(), v.capacity());
    len <= cap && cap < (1 << 24) && (cap == 0 || core::mem::size_of::<T>() == 0 || (p >> 48) == 0xffff)
}

const EP_MAGIC: u64 = 0x4570_4361_6368_6531;

fn ep_cache_new(slot: usize, lid: u32) -> alloc::boxed::Box<EpCache> {
    alloc::boxed::Box::new(EpCache { magic: EP_MAGIC, owner: sched::current_slot_tag(), slot, inst: 0, lid,
                                     ents: alloc::vec::Vec::new(), carry: sched::WakeRing::EMPTY, cgen: 0,
                                     others: alloc::vec::Vec::new(), scratch: Default::default() })
}

fn ep_cache_sane(c: &EpCache) -> bool {
    ep_vec_sane(&c.ents) && ep_vec_sane(&c.others) && c.others.iter().all(|o| ep_vec_sane(&o.ents))
}

fn epoll_wait_ready(pid: u32, epfd: usize, slot: usize, events_ptr: usize, maxevents: usize,
                    infinite: bool, deadline: u64) -> isize {
    use core::sync::atomic::Ordering::Relaxed;
    use sched::wakestat as ws;
    ws::EP_CALLS.fetch_add(1, Relaxed);
    let self_tag = epoll_self_tag(slot);
    let (cbox, lst) = sched::poll_listen_enter();
    let inst_id = { EPOLL_INSTANCES.lock()[slot].id };
    let fresh = cbox.is_none();
    let mut c: alloc::boxed::Box<EpCache> = match cbox.and_then(|b| b.downcast::<EpCache>().ok()) {
        Some(c) if c.magic == EP_MAGIC && c.owner == sched::current_slot_tag() && ep_cache_sane(&c) => c,
        Some(c) => {
            let (m, o, here) = (c.magic, c.owner, sched::current_slot_tag());
            serial_print_str(if m != EP_MAGIC { "[EPOLL] torn wait snapshot (magic) " }
                             else if o != here { "[EPOLL] foreign wait snapshot owner slot " }
                             else { "[EPOLL] bad wait snapshot vectors " });
            mmap_print_dec(o.0); serial_print_str("/"); mmap_print_dec(o.1 as usize);
            serial_print_str(" here "); mmap_print_dec(here.0); serial_print_str("/"); mmap_print_dec(here.1 as usize);
            serial_print_str("\n");
            // A torn snapshot (Pi 5, a9a61b2b: `ents` with a NULL buffer
            // and a capacity, so ep_diff's resize memset address 0 under
            // EPOLL_INSTANCES). Never touch it again: leak it and start over.
            serial_print_str("[EPOLL] corrupt wait snapshot dropped (leaked): pid=");
            mmap_print_dec(pid as usize);
            serial_print_str("\n");
            core::mem::forget(c);
            ep_cache_new(slot, 0)
        }
        None => ep_cache_new(slot, 0),
    };
    // Another instance than the snapshot's, under the same listener: swap in
    // a kept snapshot of it if there is one (the current one is kept).
    if let Some((id, _)) = &lst {
        if c.lid == *id && !(c.slot == slot && c.inst == inst_id) {
            if let Some(k) = c.others.iter().position(|o| o.slot == slot && o.inst == inst_id) {
                let o = c.others.remove(k);
                let prev = EpSnap { slot: c.slot, inst: c.inst,
                                    ents: core::mem::replace(&mut c.ents, o.ents),
                                    carry: core::mem::replace(&mut c.carry, o.carry),
                                    cgen: c.cgen };
                c.slot = o.slot; c.inst = o.inst; c.cgen = o.cgen;
                if prev.inst != 0 { c.others.push(prev); }
            }
        }
    }
    let mut why;
    let mut pending; // re-probe the snapshot's ready list on this pass
    let mut lid;
    let mut listen_all; // the listener's filter is still ALL (no set yet)
    match lst {
        Some((id, ring)) if c.slot == slot && c.inst == inst_id && c.lid == id => {
            ep_share(&mut c.others, &ring);
            let mut r = core::mem::replace(&mut c.carry, sched::WakeRing::EMPTY);
            ep_merge(&mut r, &ring);
            lid = id; why = EpWhy::Ring(r); pending = true; listen_all = false;
        }
        _ => {
            ws::EP_MISS.fetch_add(1, Relaxed);
            // Per-thread-group miss census by cause (/proc/sysstat pseudo
            // rows, `sched::sysacct::NR_EPOLL_MISS_*`).
            let cause = if fresh { sched::sysacct::NR_EPOLL_MISS_FIRST }
                else if c.slot != slot || c.inst != inst_id { sched::sysacct::NR_EPOLL_MISS_INST }
                else { sched::sysacct::NR_EPOLL_MISS_OFF };
            sched::sysacct::note(sched::current_tgid(), cause);
            // Still listening under the snapshot's id: keep the snapshot (of
            // another instance) and record everything for this call's first
            // pass without restarting the listener, so the kept ones stay
            // valid. Otherwise start over.
            let keep = match &lst {
                Some((id, ring)) if c.lid == *id && c.inst != 0 && c.ents.iter().any(|e| e.used) => {
                    ep_share(&mut c.others, ring);
                    let mut prev = EpSnap { slot: c.slot, inst: c.inst,
                                            ents: core::mem::take(&mut c.ents),
                                            carry: core::mem::replace(&mut c.carry, sched::WakeRing::EMPTY),
                                            cgen: c.cgen };
                    ep_merge(&mut prev.carry, ring);
                    c.others.push(prev);
                    while c.others.len() > EP_OTHERS { c.others.remove(0); }
                    let mut all = sched::PollSet::new();
                    all.add_bits(sched::POLL_TAG_ALL);
                    match sched::poll_listen_update(*id, &all) {
                        Some(r) => { ep_share(&mut c.others, &r); Some(*id) }
                        None => None,
                    }
                }
                _ => None,
            };
            c.slot = slot; c.inst = inst_id; c.ents.clear(); c.carry = sched::WakeRing::EMPTY;
            lid = match keep {
                Some(id) => id,
                None => { c.others.clear(); sched::poll_listen_begin(None) }
            };
            why = EpWhy::All; pending = false; listen_all = true;
        }
    }
    c.lid = lid;
    let mut cand: alloc::vec::Vec<bool> = alloc::vec::Vec::new();
    let mut recheck: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
    let mut changed: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
    let mut carry = sched::WakeRing::EMPTY;
    let mut gd_woke = 0u64;
    let mut churn = 0u32;
    let mut woke = false; // a real sleep ended since the last park (EP_USELESS)
    let mut woke_ring = false; // that wake named registrations (else: an unattributed full re-probe)
    let mut last_ring = sched::WakeRing::EMPTY; // that wake's ring (EP_NAMELESS)
    let st = ws::ep_stat(c.inst, sched::current_tgid(), epfd as u32);
    loop {
        let gen0 = EPOLL_CTL_GEN.load(Ordering::Acquire);
        let mut dirty = false;
        // ---- DIFF ---- the instance against the snapshot: new, re-pointed
        // and MODified registrations are (re-)probed; removed ones dropped.
        {
            let ep = EPOLL_INSTANCES.lock();
            let inst = &ep[slot];
            if inst.id != c.inst {
                // The slot holds another instance now (the fd was closed
                // under us): start over.
                c.ents.clear(); c.inst = inst.id; why = EpWhy::All;
            }
            let hi = if inst.in_use { (inst.hi as usize).min(inst.interests.len()) } else { 0 };
            ep_diff(&mut c.ents, &inst.interests, hi, Some(&mut cand));
            // Nested registrations: diff their kids too (no probe, no IPC);
            // a new or MODified kid makes the registration a candidate.
            let kids_on = EPOLL_KIDS.load(Relaxed);
            for i in 0..c.ents.len() {
                let e = &mut c.ents[i];
                if !kids_on || !e.used || !e.known || e.tag.nested == 0 { continue; }
                let ks = e.tag.nested as usize - 1;
                let k = e.kids.get_or_insert_with(|| alloc::boxed::Box::new(
                    EpKids { slot: ks, inst: 0, ents: alloc::vec::Vec::new() }));
                if ep_diff_kids(k, &ep[..]) { cand[i] = true; }
            }
        }
        // ---- CANDIDATES ----
        let full = matches!(why, EpWhy::All);
        for i in 0..c.ents.len() {
            let e = &c.ents[i];
            if !e.used || !e.armed { cand[i] = false; continue; }
            let ring = match &why { EpWhy::Ring(r) => Some(r), _ => None };
            let hit = match &e.kids {
                Some(k) => ep_kids_need(k, ring),
                None => ep_always(&e.tag) || ring.map_or(false, |r| ep_hit(&e.tag, r)),
            };
            if full || !e.known || (pending && e.ready) || hit {
                cand[i] = true;
            }
        }
        for &i in recheck.iter() {
            let i = i as usize;
            if i < cand.len() && c.ents[i].used && c.ents[i].armed { cand[i] = true; }
        }
        pending = false;
        let why_now = core::mem::replace(&mut why, EpWhy::Nothing); // the kids are judged by it too
        woke_ring = matches!(why_now, EpWhy::Ring(_));
        if let EpWhy::Ring(r) = &why_now { last_ring = *r; }
        recheck.clear(); changed.clear();
        // ---- PROBE ---- only the candidates; per registration, exactly the
        // probe-everything loop's rules.
        let mut n = 0usize;
        let mut efault = false; // the events buffer could not be written
        let nested_stale = c.cgen != gen0;
        ws::EP_PASSES.fetch_add(1, Relaxed);
        let t_probe = monotonic_ns();
        for i in 0..c.ents.len() {
            if !cand[i] { continue; }
            if n >= maxevents {
                // Next call. A nested registration's kids that this pass
                // would have probed must survive the ring that named them.
                if let Some(k) = c.ents[i].kids.as_mut() {
                    let ring = match &why_now { EpWhy::Ring(r) => Some(r), _ => None };
                    ep_kids_defer(k, full, ring);
                }
                c.ents[i].ready = true;
                continue;
            }
            let it = {
                let ep = EPOLL_INSTANCES.lock();
                if ep[slot].id != c.inst || i >= ep[slot].interests.len() { continue; }
                ep[slot].interests[i]
            };
            let (efd, eofd) = (c.ents[i].fd, c.ents[i].ofd);
            if !it.in_use || !it.armed || it.fd != efd || it.ofd != eofd { continue; } // the next diff sees it
            ws::EP_PROBES.fetch_add(1, Relaxed);
            // A nested registration with kids: probe only the kids a wake or
            // their own state names (see `EpKids`), while its fd still names
            // that instance; else forget the kids and probe it whole.
            if let Some(ks) = c.ents[i].kids.as_ref().map(|k| k.slot) {
                if ep_nested_slot(pid, &it) != Some(ks) { c.ents[i].kids = None; }
            }
            if let Some(k) = c.ents[i].kids.as_mut() {
                let ring = match &why_now { EpWhy::Ring(r) => Some(r), _ => None };
                let (fire, ch) = ep_eval_kids(pid, k, full, ring, nested_stale);
                let kready = k.ents.iter().any(|x| x.used && x.armed && x.ready);
                let e = &mut c.ents[i];
                if ch { dirty = true; if !listen_all { changed.push(i as u32); } }
                e.ready = kready;
                // Readable while a kid fires (the inner ready list is not
                // empty); never writable, no edge source: level, like the
                // whole-instance probe.
                let cur = if fire { POLLIN_EP & it.events } else { 0 };
                if cur != 0 {
                    let off = n * EPOLL_EVENT_SIZE;
                    if !(ua::write_user::<u32>(events_ptr + off, cur)
                        && ua::write_user::<u64>(events_ptr + off + EPOLL_EVENT_DATA_OFF, it.data)) {
                        // Unwritable events buffer: EFAULT (or the events
                        // already delivered); the entry stays ready.
                        efault = true; c.ents[i].ready = true; break;
                    }
                    if gd_stats() { gd_fd(it.fd as usize, 0); }
                    if it.events & EPOLLONESHOT != 0 {
                        let mut ep = EPOLL_INSTANCES.lock();
                        if let Some(j) = ep[slot].interests.iter().position(|x| x.is(it.fd, it.ofd)) {
                            ep[slot].interests[j].armed = false;
                        }
                        drop(ep);
                        c.ents[i].armed = false;
                    }
                    n += 1;
                }
                continue;
            }
            let (cur, seq, tag) = probe_interest(pid, slot, &it, 0);
            if CHURN_STATS && tag.bits == sched::POLL_TAG_ALL { churn_all_tag_note(pid, it.fd as usize); }
            let e = &mut c.ents[i];
            if !e.known || e.tag != tag || (nested_stale && tag.nested != 0) {
                e.tag = tag; e.known = true; dirty = true;
                if !listen_all { changed.push(i as u32); }
            }
            if cur & POLLNVAL_EP != 0 { e.ready = false; continue; } // see epoll_wait_until
            e.ready = cur != 0;
            let et = it.events & EPOLLET != 0;
            let fire = cur != 0 && (!et || match seq {
                Some(s) => s != it.last_seq,
                None    => true,
            });
            if fire {
                let off = n * EPOLL_EVENT_SIZE;
                if !(ua::write_user::<u32>(events_ptr + off, cur)
                    && ua::write_user::<u64>(events_ptr + off + EPOLL_EVENT_DATA_OFF, it.data)) {
                    efault = true; c.ents[i].ready = true; break;
                }
                if gd_stats() { gd_fd(it.fd as usize, 0); }
                let mut ep = EPOLL_INSTANCES.lock();
                if let Some(j) = ep[slot].interests.iter().position(|x| x.is(it.fd, it.ofd)) {
                    if let Some(s) = seq { ep[slot].interests[j].last_seq = s; }
                    if it.events & EPOLLONESHOT != 0 { ep[slot].interests[j].armed = false; }
                }
                drop(ep);
                if it.events & EPOLLONESHOT != 0 { c.ents[i].armed = false; }
                n += 1;
            }
        }
        // ---- FILTER ---- a new or changed answer: rebuild the listener's
        // filter, then re-probe what changed under it (a wake between that
        // probe and this install may have been filtered out). A listener
        // still on the record-everything filter always gets the real one
        // (an empty set too: it must not park broadcast).
        let t_filt = monotonic_ns();
        ws::EP_PROBE_NS.fetch_add(t_filt.saturating_sub(t_probe), Relaxed);
        if dirty || listen_all {
            let mut set = sched::PollSet::new();
            set.add_key(epoll_self_key(slot)); // epoll_ctl wakes it by this key
            for e in c.ents.iter_mut() {
                e.filt = e.used && e.armed && e.known;
                if let Some(k) = e.kids.as_mut() {
                    for x in k.ents.iter_mut() { x.filt = x.used && x.armed && x.known; }
                }
                if e.filt { ep_filter_add(&mut set, pid, e); }
            }
            // The kept snapshots' registrations stay covered (see `others`).
            for o in c.others.iter() {
                set.add_key(epoll_self_key(o.slot));
                for e in o.ents.iter() {
                    if e.filt { ep_filter_add(&mut set, pid, e); }
                }
            }
            c.cgen = gen0;
            match sched::poll_listen_update(lid, &set) {
                Some(r) => { ep_share(&mut c.others, &r); ep_merge(&mut carry, &r); listen_all = false; }
                None => {
                    // Not listening any more (cannot happen inside a call:
                    // only this thread's own parks end it). Start over —
                    // but events already consumed above are returned first.
                    c.others.clear();
                    lid = sched::poll_listen_begin(None); c.lid = lid; listen_all = true;
                    if n == 0 { why = EpWhy::All; continue; }
                    for e in c.ents.iter_mut() { e.ready = true; }
                }
            }
            core::mem::swap(&mut recheck, &mut changed);
            ws::EP_FILT_NS.fetch_add(monotonic_ns().saturating_sub(t_filt), Relaxed);
        }
        let timed_out = !infinite && monotonic_ns() >= deadline;
        if gd_stats() {
            gd_site(core::mem::take(&mut gd_woke), n > 0 || timed_out || interrupted(), false);
            if n > 0 { gd_inc(K_EP_EV); } else if timed_out { gd_inc(K_EP_TMO); }
            else if interrupted() { gd_inc(K_EP_INTR); }
        }
        if n > 0 || efault || timed_out || interrupted() {
            if n > 0 && woke { st.useful.fetch_add(1, Relaxed); }
            // Keep the snapshot (and the listener) for the next call: what
            // was not acted on yet is re-probed then.
            for &i in recheck.iter() { if let Some(e) = c.ents.get_mut(i as usize) { e.ready = true; } }
            c.carry = carry;
            let r = if n > 0 { n as isize } else if efault { -14 } else if timed_out { 0 } else { -4 };
            sched::poll_listen_store(Some(c));
            return r;
        }
        if !carry.is_empty() {
            why = EpWhy::Ring(core::mem::replace(&mut carry, sched::WakeRing::EMPTY));
            continue;
        }
        if !recheck.is_empty() {
            ws::EP_RECHECK.fetch_add(1, Relaxed);
            // An answer that keeps changing (it should not: tags are per
            // object) must not spin here: past two rechecks, probe under
            // the record-everything filter, which needs no recheck.
            churn += 1;
            if churn > 2 && !listen_all {
                let mut all = sched::PollSet::new();
                all.add_bits(sched::POLL_TAG_ALL);
                match sched::poll_listen_update(lid, &all) {
                    Some(r) => { ep_share(&mut c.others, &r); ep_merge(&mut carry, &r); listen_all = true; }
                    None => {
                        c.others.clear();
                        lid = sched::poll_listen_begin(None); c.lid = lid; listen_all = true;
                        why = EpWhy::All;
                    }
                }
            }
            continue;
        }
        // ---- BLOCK ---- unless a wake is already waiting (one RUN_QUEUE
        // hold, so none falls between the look and the sleep).
        if woke {
            ws::EP_USELESS.fetch_add(1, Relaxed); woke = false;
            // Attribute the useless wake: the registrations this pass probed
            // (the ones the wake's ring named), else the unattributed bucket.
            st.useless.fetch_add(1, Relaxed);
            let mut named = false;
            if woke_ring {
                for i in 0..cand.len().min(c.ents.len()) {
                    if cand[i] && c.ents[i].used { st.note_fd(c.ents[i].fd as u32); named = true; }
                }
            } else { st.note_fd(ws::EpStat::ALL_FD); }
            ws::EP_NAMELESS[ep_nameless_why(woke_ring, named, &last_ring, self_tag, &c.others, &c.ents)]
                .fetch_add(1, Relaxed);
        }
        match sched::poll_listen_park(lid, if infinite { u64::MAX } else { deadline }) {
            sched::ListenPark::Taken(r) => { ep_share(&mut c.others, &r); why = EpWhy::Ring(r); }
            sched::ListenPark::Off => {
                c.others.clear();
                lid = sched::poll_listen_begin(None); c.lid = lid; listen_all = true;
                why = EpWhy::All;
            }
            sched::ListenPark::Parked => {
                if EPOLL_CTL_GEN.load(Ordering::Acquire) != gen0 || interrupted()
                    || (!infinite && monotonic_ns() >= deadline) {
                    if EPOLL_CTL_GEN.load(Ordering::Acquire) != gen0 { ws::EP_GENCANCEL.fetch_add(1, Relaxed); }
                    sched::block_on_poll_cancel();
                    continue;
                }
                ws::EP_PARKS.fetch_add(1, Relaxed);
                st.parks.fetch_add(1, Relaxed);
                sched::sysacct::note(sched::current_tgid(), sched::sysacct::NR_EPOLL_PARK);
                if gd_stats() { gd_inc(K_EP_PARK); }
                // The snapshot lives in the Task while asleep: a thread killed
                // here is reaped without unwinding this frame.
                c.scratch = (core::mem::take(&mut cand), core::mem::take(&mut recheck), core::mem::take(&mut changed));
                sched::ep_cache_put(c);
                sched::block_on_poll_commit();
                c = match sched::ep_cache_get().and_then(|b| b.downcast::<EpCache>().ok()) {
                    Some(c) if c.magic == EP_MAGIC && c.owner == sched::current_slot_tag() => c,
                    Some(c) => {
                        // Lane piepollsrc: what went to sleep in this slot is
                        // not what came back.
                        serial_print_str("[EPOLL] wait snapshot changed across park: pid=");
                        mmap_print_dec(pid as usize);
                        serial_print_str(if c.magic != EP_MAGIC { " torn\n" } else { " foreign\n" });
                        core::mem::forget(c);
                        ep_cache_new(slot, lid)
                    }
                    None => ep_cache_new(slot, lid),
                };
                (cand, recheck, changed) = core::mem::take(&mut c.scratch);
                woke = true;
                if gd_stats() { gd_woke = sched::gdwake::take(pid); }
                // The ring the wake handed over in the slot mailbox (lane
                // percpu4: no RUN_QUEUE hold). Wakes after the hand-over wait
                // in the ring for the next park, which takes them in its own
                // hold; a listener that is gone shows up there as `Off`.
                let r = sched::poll_listen_woken(lid);
                why = if !r.is_empty() {
                    ep_share(&mut c.others, &r); EpWhy::Ring(r)
                } else {
                    ws::EP_UNKNOWN.fetch_add(1, Relaxed);
                    ep_share_unknown(&mut c.others);
                    EpWhy::All
                };
            }
        }
    }
}

/// Ctrl-T: list every epoll instance's interests (diagnostic, off by default:
/// it is one line per live instance).
const EPOLL_DUMP_INTERESTS: bool = false;

/// `poll(2)`'s POLLNVAL as the probes report it for an fd that is not open.
const POLLNVAL_EP: u32 = 0x0020;
/// `poll(2)`'s POLLIN: what a nested epoll fd reports while it has events.
const POLLIN_EP: u32 = 0x0001;

/// Release `fd`'s epoll registrations in thread group `tgid` because `fd` is
/// about to be closed (or overwritten by dup2/dup3). Call it BEFORE the close.
///
/// Linux keys an epoll item by (file, fd) and removes it when the *file* is
/// released (`eventpoll_release`): a closed fd never wakes or reports anything
/// again, and `epoll_wait` has no EPOLLNVAL at all. This table keys interests
/// by fd number only and nothing removed them on close, so the probe asked
/// about a dead number, got EBADF, and reported POLLNVAL to every waiter of
/// the instance — and once the number was reused, probed the new file under
/// the old registration.
///
/// That is what made closing cosmic-term windows crash (lane termsegv,
/// 2026-10-01). On exit the main thread disconnects its `wl_display` (closes
/// the Wayland socket) and frees it while iced's SCTK event-loop thread is
/// parked in epoll_wait on the same socket. The close woke that thread,
/// epoll_wait handed it the socket with POLLNVAL, and it dispatched through
/// the freed display: a write to NULL+0x254 in `wl_map_insert_at`, SIGSEGV.
/// On Linux the thread sleeps on until exit_group takes it. (The thread can
/// still lose the same race when real compositor traffic wakes it during the
/// teardown — that use-after-free is the application's, and Linux has it too.)
///
/// If another fd of the same process still names the same open object (a
/// dup), the registration moves to that fd instead of being dropped — on
/// Linux the file is still alive and keeps reporting. A file kept alive only
/// by another process (a forked child's copy) is dropped; this table cannot
/// probe it through this process's fds.
///
/// Since lane epollofd (2026-10-01) this only concerns registrations with no
/// open file description (`EpollInterest::ofd == 0`: untracked console fds,
/// a full description table). Every other registration is keyed by its
/// description and removed when the description's LAST reference goes
/// (`epoll_ofd_released`) — which is what Linux does, and what keeps a
/// registration alive across close() while a dup, a fork child or an
/// SCM_RIGHTS receiver still holds the file.
fn epoll_release_fd(pid: u32, tgid: u32, fd: usize) {
    if !epoll_watches_fd(tgid, fd) { return; }
    // Outside EPOLL_INSTANCES: the alias lookup takes the VFS fd-table lock.
    let alias = if fd < net_server::SOCK_FD_BASE {
        vfs::vfs_get_node_kind(pid, fd).and_then(|k| vfs::find_alias_fd(pid, &k, fd))
    } else { None };
    let mut ep = EPOLL_INSTANCES.lock();
    let mut changed = false;
    for inst in ep.iter_mut() {
        if !inst.in_use || inst.owner_tgid != tgid { continue; }
        let n = inst.hi as usize;
        let alias_free = alias.map_or(false, |a| !inst.interests[..n].iter().any(|x| x.is(a as i32, 0)));
        for it in inst.interests[..n].iter_mut() {
            if it.is(fd as i32, 0) {
                match alias {
                    Some(a) if alias_free => it.fd = a as i32,
                    _ => *it = EpollInterest::empty(),
                }
                changed = true;
            }
        }
    }
    if changed { EPOLL_CTL_GEN.fetch_add(1, Ordering::AcqRel); }
}

/// `fd`'s description in `tgid` when `fd` is open there (`Some(0)`: open but
/// without a description — an untracked console fd 0-2, an epoll fd, a
/// number outside the checked ranges), None when it is not open (EBADF).
fn fd_open_ofd(tgid: u32, fd: usize) -> Option<u32> {
    if fd <= 2 && !vfs::fd_redirected(tgid, fd) { Some(0) }
    else if fd <= 2 { vfs::fd_ofd(tgid, fd) }
    else if fd < net_server::SOCK_FD_BASE { vfs::fd_ofd(tgid, fd) }
    else if fd < EPOLL_FD_BASE { net_server::fd_ofd(tgid, fd) }
    else if epoll_fd_index(fd).is_some() { epoll_slot_for(fd, tgid).map(|_| 0) }
    else { Some(0) }
}

/// `vfs::ofd` release hook: the last reference to description `id` is gone,
/// so every epoll item on it goes (Linux `eventpoll_release`). Runs with no
/// fd-table or epoll lock held. Only called for descriptions epoll_ctl
/// registered (`vfs::ofd::mark_watched`), so an ordinary close never pays
/// for the scan.
fn epoll_ofd_released(id: u32) {
    let mut ep = EPOLL_INSTANCES.lock();
    let mut changed = false;
    for inst in ep.iter_mut() {
        if !inst.in_use { continue; }
        let n = inst.hi as usize;
        for it in inst.interests[..n].iter_mut() {
            if it.in_use && it.ofd == id { *it = EpollInterest::empty(); changed = true; }
        }
    }
    if changed { EPOLL_CTL_GEN.fetch_add(1, Ordering::AcqRel); }
}

/// Probe one registration of instance `slot` for waiter `pid`.
///
/// A registration without a description (`ofd == 0`) is probed by number, as
/// it always was. Otherwise it is probed through an fd that names its
/// description: the owner's own `fd` while it still does — the common case,
/// one VFS_POLL/NET_POLL round trip with the description check folded in —
/// else the holder cached in the registration, else one found by
/// `vfs::ofd_holder`/`net_server::ofd_holder` (a dup in the owner, a fork
/// child, an SCM_RIGHTS receiver), which is then cached. So a closed number
/// never reports POLLNVAL, and a number reused for another file is never
/// probed under the old registration.
///
/// A description that is still referenced but by no fd at all (only by an
/// SCM_RIGHTS message still in a socket queue) is probed through the queued
/// descriptor (`net_server::inflight_vfs`), as Linux keeps reporting it. A
/// queued *socket* cannot be probed that way: it reports nothing and
/// contributes the broadcast tag, so its first fd's events are not lost once
/// it is received.
fn probe_interest(pid: u32, slot: usize, it: &EpollInterest, depth: u32) -> (u32, Option<u64>, sched::PollTag) {
    if it.ofd == 0 {
        return probe_fd_events_seq_nested(pid, it.fd as usize, it.events, depth);
    }
    let (ppid, pfd) = if it.ptgid != 0 { (it.ptgid, it.pfd as usize) } else { (pid, it.fd as usize) };
    if let Some(r) = probe_fd_ofd(ppid, pfd, it.events, depth, it.ofd) { return r; }
    let owner = { EPOLL_INSTANCES.lock()[slot].owner_tgid };
    let holder = if (it.fd as usize) < net_server::SOCK_FD_BASE { vfs::ofd_holder(owner, it.ofd) }
                 else { net_server::ofd_holder(owner, it.ofd) };
    match holder {
        Some((t, f)) if f <= u32::MAX as usize => {
            {
                let mut ep = EPOLL_INSTANCES.lock();
                let n = ep[slot].hi as usize;
                if let Some(x) = ep[slot].interests[..n].iter_mut().find(|x| x.is(it.fd, it.ofd)) {
                    if t == owner && f == it.fd as usize { x.ptgid = 0; x.pfd = 0; }
                    else { x.ptgid = t; x.pfd = f as u32; }
                }
            }
            probe_fd_ofd(t, f, it.events, depth, it.ofd).unwrap_or((0, None, sched::PollTag::ALL))
        }
        _ if vfs::ofd::live(it.ofd) => {
            // No fd names it: it is held only by an SCM_RIGHTS message still
            // queued on a socket. Linux keeps reporting such an item, so probe
            // the queued descriptor's object itself. The copy holds no
            // reference, so the answer counts only if the description is
            // still alive afterwards (else its object may have been recycled).
            if (it.fd as usize) < net_server::SOCK_FD_BASE {
                if let Some(tf) = net_server::inflight_vfs(it.ofd) {
                    let (st, seq, tag) = vfs::poll_transfer(&tf, pid);
                    if vfs::ofd::live(it.ofd) {
                        const POLLERR: u32 = 0x0008;
                        const POLLHUP: u32 = 0x0010;
                        return ((st & it.events) | (st & (POLLERR | POLLHUP)), Some(seq), sched::PollTag::bits(tag));
                    }
                }
            }
            (0, None, sched::PollTag::ALL)
        }
        _ => {
            // Released without the hook reaching this item (it raced the
            // ADD): drop it now.
            epoll_ofd_released(it.ofd);
            (POLLNVAL_EP, None, sched::PollTag::NONE)
        }
    }
}

/// `probe_fd_events_seq_nested` for an fd that must still name description
/// `ofd`: `None` when it is closed or names another one.
fn probe_fd_ofd(pid: u32, fd: usize, requested: u32, depth: u32, ofd: u32) -> Option<(u32, Option<u64>, sched::PollTag)> {
    probe_fd_events_seq_inner(pid, fd, requested, depth, ofd)
}

/// `epoll_release_fd` for every fd in `lo..=hi` (close_range), without the
/// alias search: the whole range is going.
fn epoll_drop_range(tgid: u32, lo: usize, hi: usize) {
    let mut ep = EPOLL_INSTANCES.lock();
    let mut changed = false;
    for inst in ep.iter_mut() {
        if !inst.in_use || inst.owner_tgid != tgid { continue; }
        let n = inst.hi as usize;
        for it in inst.interests[..n].iter_mut() {
            if it.in_use && it.ofd == 0 && it.fd >= 0 && (lo..=hi).contains(&(it.fd as usize)) {
                *it = EpollInterest::empty();
                changed = true;
            }
        }
    }
    if changed { EPOLL_CTL_GEN.fetch_add(1, Ordering::AcqRel); }
}

/// Does any epoll instance of `tgid` have a registration on `fd`? (The cheap
/// pre-check that keeps an ordinary close from paying for the alias search.)
fn epoll_watches_fd(tgid: u32, fd: usize) -> bool {
    let ep = EPOLL_INSTANCES.lock();
    ep.iter().any(|inst| inst.in_use && inst.owner_tgid == tgid
        && inst.interests[..inst.hi as usize].iter().any(|i| i.is(fd as i32, 0)))
}

/// Read-only readiness check for the block-loop re-probe: like the epoll_wait
/// probe but mutates no `last_seq` and writes no user memory, so cancelling the
/// block and looping re-delivers the edge instead of consuming it silently.
fn epoll_any_ready(pid: u32, slot: usize) -> bool { epoll_any_ready_nested(pid, slot, 0) }

/// `epoll_any_ready` with the nested-epoll recursion depth threaded through.
/// Also the readiness definition for an epoll fd watched by an OUTER epoll or
/// poll/select (see `poll_fd_state_nested`).
fn epoll_any_ready_nested(pid: u32, slot: usize, depth: u32) -> bool {
    let hi = { EPOLL_INSTANCES.lock()[slot].hi as usize };
    for i in 0..hi {
        let interest = { EPOLL_INSTANCES.lock()[slot].interests[i] };
        if !interest.in_use || !interest.armed { continue; }
        if depth > 0 { sched::wakestat::EP_INNER.fetch_add(1, Ordering::Relaxed); }
        let (cur, seq, _tag) = probe_interest(pid, slot, &interest, depth);
        if cur & POLLNVAL_EP != 0 { continue; } // closed fd: never ready (see epoll_wait_until)
        let et = interest.events & EPOLLET != 0;
        let fire = cur != 0 && (!et || match seq {
            Some(s) => s != interest.last_seq,
            None    => true,
        });
        if fire { return true; }
    }
    false
}

/// 100 Hz poll-deadline tick hook (registered from kernel init, runs in BSP
/// timer-IRQ context). Wakes all pollers once the earliest finite poll/select
/// timeout or armed-timerfd expiry is reached — so an epoll_wait(-1) waiter on
/// a timerfd, and every finite-timeout waiter, wakes on time with NO per-tick
/// wake for infinite data-driven waiters (the ~0% idle goal). try_wake_poll
/// honors the tick's try-lock-only contract: a contended tick defers ≤10 ms.
/// [GAP2] 0.5 Hz content probe of the watched wl_shm pool.
///
/// IRQ CONTEXT (BSP timer tick, via `sched::timer_tick_irq`). It may only:
/// try_lock, read HHDM (kernel) memory, and write the UART directly. It must
/// NOT touch user memory, must NOT take RUN_QUEUE, and must NOT wake anything.
fn gap2_sample_tick() {
    use core::sync::atomic::Ordering::Relaxed;
    if !mm::gap2::ON { return; }
    if !mm::gap2::watch_any() { return; }
    static LAST: AtomicUsize = AtomicUsize::new(0);
    let now = ticks() as usize;                  // 100 Hz
    if now.wrapping_sub(LAST.load(Relaxed)) < 50 { return; }   // 0.5 Hz
    LAST.store(now, Relaxed);
    // Every armed pool is sampled in the SAME tick window and stamped with the
    // same `t=`, so the applet pool and the panel's bar pools sit on one shared
    // timeline and can be compared sample-for-sample.
    for s in 0..mm::gap2::WATCH_SLOTS {
        let idx = mm::gap2::watch_slot(s);
        if idx == usize::MAX { continue; }
        if let Some((np, p0, sum, vlen)) = vfs::vmo_debug_probe(idx) {
            mm::gap2::s("[G2SUM] t="); mm::gap2::h(now);
            mm::gap2::kv(" idx=",  idx);
            mm::gap2::kv(" np=",   np);
            mm::gap2::kv(" p0=",   p0);
            mm::gap2::kv(" vlen=", vlen);
            mm::gap2::kv(" sum=",  sum as usize);
            mm::gap2::nl();
        }
    }
}

/// Master gate for the `[EVSTAT]` evdev I/O census (`evstat_tick`). Same shape
/// as `DRM_STATS`: a const so the whole sampler compiles out when off.
///
/// OFF. On at 0.5 Hz this writes ~366 bytes of two census lines into every
/// captured log forever — it accounted for 28,900 of the 30,934 console bytes
/// of a 100-line measurement — which buries the output under test and inflates
/// any count taken over a log. Turn it on to investigate; it is not a shipping
/// diagnostic. Note that `artifacts/m15_serial_stall.py` needs it on: its guard
/// works by having the timer IRQ print, and with the census off nothing reaches
/// `putc` from IRQ context for a parked serial reader to back-pressure.
pub const EV_STATS: bool = false;

/// 0.5 Hz `[EVSTAT]` census of every evdev node, one line per device.
///
/// WHY IT EXISTS. `evpush` on the `[DRMSTAT]` line already witnesses that
/// host-injected input reaches the kernel ring. It cannot distinguish the two
/// ways a compositor can then ignore it: never opening `/dev/input/eventN`, and
/// opening it but never reading. `reads`/`polls`/`ioctls` are nonzero only if
/// some process is actually talking to the node, `deliv` only if events left
/// the kernel, and `drop` climbing with `deliv` flat is a ring nobody drains.
/// `conspop` is broken out separately because the in-kernel console pops
/// event0 for its own keyboard input — the console and a compositor are
/// competing consumers of the same keyboard ring.
///
/// IRQ CONTEXT (BSP timer tick, via `poll_deadline_tick`). Relaxed atomic loads
/// plus one `try_lock` for the ring depth, and direct UART writes. It takes no
/// blocking lock, touches no user memory and never takes `RUN_QUEUE`. A depth
/// of `0xffffffffffffffff` means the depth sample was MISSED on lock
/// contention, not that the ring is empty.
fn evstat_tick() {
    use core::sync::atomic::Ordering::Relaxed;
    if !EV_STATS { return; }
    static LAST: AtomicUsize = AtomicUsize::new(0);
    let now = ticks() as usize;                        // 100 Hz
    if now.wrapping_sub(LAST.load(Relaxed)) < 200 { return; }   // 0.5 Hz
    LAST.store(now, Relaxed);
    for dev in 0..2usize {
        let c = evdev_server::census(dev);
        mm::gap2::s("[EVSTAT] t=");   mm::gap2::h(now);
        mm::gap2::kv(" dev=",     dev);
        mm::gap2::kv(" push=",    c.pushed  as usize);
        mm::gap2::kv(" drop=",    c.dropped as usize);
        mm::gap2::kv(" depth=",   c.depth   as usize);
        mm::gap2::kv(" clients=", c.clients as usize);
        mm::gap2::kv(" reads=",   c.reads   as usize);
        mm::gap2::kv(" eagain=",  c.eagain  as usize);
        mm::gap2::kv(" deliv=",   c.deliv   as usize);
        mm::gap2::kv(" polls=",   c.polls   as usize);
        mm::gap2::kv(" pollin=",  c.pollin  as usize);
        mm::gap2::kv(" ioctls=",  c.ioctls  as usize);
        mm::gap2::kv(" enotty=",  c.enotty  as usize);
        mm::gap2::kv(" lastnr=",  c.lastnr  as usize);
        mm::gap2::kv(" conspop=", c.conspop as usize);
        mm::gap2::kv(" rpid=",    c.rpid    as usize);
        mm::gap2::kv(" ipid=",    c.ipid    as usize);
        mm::gap2::nl();
    }
    // Per-CLIENT census. The per-device totals above cannot show this class of
    // defect at all: `push=128 deliv=16` reads identically for one starved
    // reader and for sixteen well-fed ones, and telling those apart is the
    // whole reason the per-open queues exist. One `try_lock`, ~512 B of stack,
    // no allocation — safe from this tick (IRQ) context.
    {
        let mut rows = [evdev_server::EvClientCensus::empty();
                        evdev_server::MAX_CLIENTS];
        let n = evdev_server::clients_census(&mut rows);
        if n == usize::MAX {
            // MISSED, which must not print the same as "no clients".
            mm::gap2::s("[EVCLI] t="); mm::gap2::h(now);
            mm::gap2::s(" busy\n");
        } else {
            for r in rows.iter().take(n) {
                mm::gap2::s("[EVCLI] t=");    mm::gap2::h(now);
                mm::gap2::kv(" dev=",     r.dev_id  as usize);
                mm::gap2::kv(" open=",    r.open_id as usize);
                mm::gap2::kv(" pid=",     r.pid     as usize);
                mm::gap2::kv(" vt=",      r.vt      as usize);
                mm::gap2::kv(" queued=",  r.queued  as usize);
                mm::gap2::kv(" deliv=",   r.deliv   as usize);
                mm::gap2::kv(" dropped=", r.dropped as usize);
                mm::gap2::nl();
            }
        }
    }
    if drivers::virtio_keyboard::VQ_STATS {
        let v = drivers::virtio_keyboard::vq_census();
        mm::gap2::s("[VQSTAT] t=");   mm::gap2::h(now);
        mm::gap2::kv(" polls=",   v.polls   as usize);
        mm::gap2::kv(" skips=",   v.skips   as usize);
        mm::gap2::kv(" drained=", v.drained as usize);
        mm::gap2::kv(" maxb=",    v.maxb    as usize);
        mm::gap2::kv(" minfree=", v.minfree as usize);
        mm::gap2::kv(" starve=",  v.starve  as usize);
        mm::gap2::kv(" aidx=",    v.aidx    as usize);
        mm::gap2::kv(" uidx=",    v.uidx    as usize);
        mm::gap2::kv(" notify=",  v.notify  as usize);
        mm::gap2::nl();
    }
}

/// `[SCPATH]`: the path of a stat() by the second focus (cosmic-greeter-login),
/// the first 300 and then every 500th — enough to name a stat loop's target.
fn sc_stat_path_focus() -> bool {
    let f2 = sched::SC_FOCUS2_TGID.load(Ordering::Relaxed);
    f2 != 0 && sched::current_tgid() == f2
}
fn sc_stat_path_trace(path: &[u8], r: isize, stat_ns: u64, isdir_ns: u64, open_ns: u64) {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    if n >= 300 && n % 500 != 0 { return; }
    mm::gap2::s("[SCPATH] n="); mm::gap2::h(n);
    mm::gap2::kv(" r=", r as usize);
    mm::gap2::kv(" stat_us=", (stat_ns / 1000) as usize);
    mm::gap2::kv(" isdir_us=", (isdir_ns / 1000) as usize);
    mm::gap2::kv(" open_us=", (open_ns / 1000) as usize);
    mm::gap2::kv(" t=", ticks() as usize);
    mm::gap2::kv(" pid=", current_pid() as usize);
    mm::gap2::s(" ");
    for &b in path.iter().take(160) { mm::gap2::s(if b.is_ascii_graphic() { core::str::from_utf8(core::slice::from_ref(&b)).unwrap_or("?") } else { "?" }); }
    mm::gap2::nl();
}

/// 0.5 Hz `[SCSTAT]` top-syscalls line for the focus tgid, and a 0.1 Hz
/// `[TASK]` census of every live task (state, wait object, last syscall).
/// IRQ context: atomics + one RUN_QUEUE try_lock + UART-direct output.
fn scstat_tick() {
    use core::sync::atomic::Ordering::Relaxed;
    if !SC_STATS { return; }
    static LAST: AtomicUsize = AtomicUsize::new(0);
    static ROUND: AtomicUsize = AtomicUsize::new(0);
    let now = ticks() as usize;
    if now.wrapping_sub(LAST.load(Relaxed)) < 200 { return; }
    LAST.store(now, Relaxed);
    let focus = sched::SC_FOCUS_TGID.load(Relaxed);
    let focus2 = sched::SC_FOCUS2_TGID.load(Relaxed);
    for (k, f) in [(0usize, focus), (1usize, focus2)] {
        if f == 0 { continue; }
        // Top 12 by cumulative ns, unsorted scan (512 slots, 12 passes).
        mm::gap2::s("[SCSTAT] t="); mm::gap2::h(now);
        mm::gap2::kv(" tgid=", f as usize);
        let mut taken = [false; SC_SLOTS];
        for _ in 0..12 {
            let mut best = 0usize; let mut best_ns = 0u64;
            for i in 0..SC_SLOTS {
                if taken[i] { continue; }
                let v = SC_NS[k][i].load(Relaxed);
                if v > best_ns { best_ns = v; best = i; }
            }
            if best_ns == 0 { break; }
            taken[best] = true;
            mm::gap2::s(" nr"); mm::gap2::h(best);
            mm::gap2::s(":"); mm::gap2::h(SC_N[k][best].load(Relaxed) as usize);
            mm::gap2::s(":"); mm::gap2::h((best_ns / 1000) as usize);
            mm::gap2::s(":"); mm::gap2::h((SC_MAX[k][best].load(Relaxed) / 1000) as usize);
        }
        mm::gap2::nl();
    }
    let r = ROUND.fetch_add(1, Relaxed);
    // Ask for the focus process's VMA map ~20 s after it appears (its libraries
    // are loaded by then), once.
    if focus != 0 && r >= 10 && SC_VMA_DUMP.load(Relaxed) == 0 { SC_VMA_DUMP.store(1, Relaxed); }
    // Once, ~40 s after focus: stack scan of every parked thread in the group.
    if focus != 0 && r >= 20 && SC_VMA_DUMP.load(Relaxed) == 2 {
        if sched::dump_group_stacks(focus, 1024) { SC_VMA_DUMP.store(3, Relaxed); }
    }
    if r % 5 == 0 {
        let mut rows = [sched::TaskCensusRow::empty(); 96];
        let n = sched::task_rows(&mut rows);
        if n == usize::MAX {
            mm::gap2::s("[TASK] t="); mm::gap2::h(now); mm::gap2::s(" busy\n");
        } else {
            for row in rows.iter().take(n) {
                let l = LAST_SC[(row.pid as usize) & 1023].load(Relaxed);
                mm::gap2::s("[TASK] t="); mm::gap2::h(now);
                mm::gap2::kv(" pid=", row.pid as usize);
                mm::gap2::kv(" tgid=", row.tgid as usize);
                mm::gap2::kv(" st=", row.state as usize);
                mm::gap2::kv(" bo=", row.blocked_on as usize);
                mm::gap2::kv(" pd=", row.poll_deadline as usize);
                mm::gap2::kv(" fut=", row.blocked_futex);
                mm::gap2::kv(" cpu=", row.on_cpu as usize);
                if (l >> 32) as u32 == row.pid {
                    mm::gap2::kv(" sc=", ((l >> 1) & 0x7FFF_FFFF) as usize);
                    mm::gap2::kv(" in=", (l & 1) as usize);
                }
                mm::gap2::kv(" rip=", row.urip as usize);
                mm::gap2::kv(" rsp=", row.ursp as usize);
                if row.futex_val != u64::MAX { mm::gap2::kv(" fval=", row.futex_val as usize); }
                mm::gap2::nl();
            }
        }
    }
}

/// Wake every timed waiter (poll/select/epoll/futex/nanosleep/…) and timerfd
/// whose deadline is `<= now`, and return the next pending deadline
/// (`u64::MAX` = none). Runs from the 100 Hz BSP tick (`poll_deadline_tick`)
/// AND from any CPU's one-shot deadline interrupt (`sched::timer_deadline_irq`,
/// armed by `sched::register_poll_deadline`), so a timed wait is released
/// within interrupt latency of its deadline instead of at the next tick edge.
/// IRQ context, try-lock only (the tick-hook contract).
pub fn poll_deadline_service(now: u64) -> u64 {
    use core::sync::atomic::Ordering::Relaxed;
    let mut tfd = vfs::earliest_timerfd_deadline();
    // POSIX timers / itimers: post the signal (and wake its parked target)
    // from here, not at the owner's next syscall return. A contended pass
    // leaves the timer due, so the retry below re-arms 200 µs out.
    if tty_server::earliest_timer_deadline() <= now {
        tty_server::service_timers_irq(now);
    }
    // Fast path: the lock-free hint (min of parked timed waiters' deadlines)
    // and the timerfd pool say nothing is due → no run-queue scan, no wake.
    let due = core::cmp::min(sched::NEXT_POLL_DEADLINE.load(Relaxed), tfd);
    if due != u64::MAX && now >= due && !sched::percpu::deadline_service_enter() {
        // Lane percpu: another CPU is running the pass and now owes one more
        // (`DL_AGAIN`), which serves what made this timer fire; it also
        // re-arms for the global next deadline, so this CPU stops chasing it.
        let t = tty_server::earliest_timer_deadline();
        return if t <= now { now + 200_000 } else { t };
    }
    if due != u64::MAX && now >= due {
      let mut now = now;
      let mut passes = 0u32;
      loop {
        // Something is due. First FOLD any timerfd that has reached its deadline
        // into a single expiry edge: a folded one-shot is disarmed here so it
        // stops being reported by earliest_timerfd_deadline every tick. Without
        // this, an armed one-shot whose owner is briefly descheduled sits
        // `armed` with a past deadline forever, `timerfd_due` fires every tick,
        // and the mass-wake starves that owner so it never runs to consume it —
        // the idle-desktop wake storm that stopped ext-idle-notify's `Idled`
        // from ever firing. `timerfd_due` is now "a timerfd expired on THIS
        // pass", not "a stale deadline is still <= now".
        let mut keys = sched::WakeKeys::new();
        let timerfd_tags = if tfd != u64::MAX && now >= tfd {
            vfs::fold_expired_timerfds_keyed(now, &mut keys)
        } else {
            0
        };
        // Wake the poll-channel waiters whose PER-TASK deadline has passed (or
        // all, if a timerfd expired this pass), and republish the hint to the
        // EXACT earliest remaining deadline — all under one RUN_QUEUE hold. The
        // per-task deadline is the authority, so unlike the old single-global
        // `store(u64::MAX)` this can't clobber a deadline a concurrent waiter is
        // registering (M7 lost-wake). A contended tick retries the per-task
        // deadlines next tick by itself — they are still `<= now` then — but
        // the timerfd edge is NOT retried: the fold above already disarmed a
        // one-shot (advanced a periodic), so `earliest_timerfd_deadline` stops
        // reporting it and the waiter would sleep until its own timeout. Hand
        // the expired timerfds' tags to the deferred-wake path, which the next
        // tick pays with `try_wake_poll_tagged` (and re-arms on contention).
        if !sched::service_poll_deadlines_keyed(now, timerfd_tags, &keys) && timerfd_tags != 0 {
            match keys.exact() {
                Some(ks) => for &k in ks { sched::request_poll_wake_key(k); },
                None => sched::request_poll_wake_tagged(timerfd_tags),
            }
        }
        passes += 1;
        if !sched::percpu::deadline_service_exit() { break; }
        if passes >= 4 {
            // Budget spent with requests still coming: let go; the re-arm
            // below (the hint is still due if anything is) retries shortly.
            sched::percpu::deadline_service_release();
            break;
        }
        now = monotonic_ns();
        tfd = vfs::earliest_timerfd_deadline();
      }
    }
    let next = core::cmp::min(sched::NEXT_POLL_DEADLINE.load(Relaxed),
                              vfs::earliest_timerfd_deadline())
        .min(tty_server::earliest_timer_deadline());
    // Still due after the service = the RUN_QUEUE try-lock lost (the hint was
    // left in place for a retry). Retry shortly rather than immediately — an
    // immediate re-arm would be an interrupt storm against the lock holder —
    // and never later than the tick would have.
    if next <= now { now + 200_000 } else { next }
}

pub fn poll_deadline_tick() {
    gap2_sample_tick();
    evstat_tick();
    scstat_tick();
    churn_tick();
    sched::idlestat::tick(monotonic_ns());
    gd_tick();
    poll_deadline_service(monotonic_ns());
    // Pay any wake a pipe deferred because it only advanced an object's edge
    // `seq` without changing its readable/writable level (see
    // `sched::request_poll_wake` and the pipe arms in `servers/vfs`). This is
    // NOT the safety net below: it fires only when some writer actually asked
    // for it, so an idle system still takes zero wakes per tick, and a burst of
    // thousands of small writes costs one herd per tick instead of one each.
    sched::service_deferred_poll_wake();
    // Bring-up safety net (design §1), OFF: a periodic unconditional wake would
    // turn any missed edge site into a ≤100 ms latency blip instead of a hang.
    // Kept false so the idle-CPU test proves edge coverage is complete.
    const POLL_SAFETY_WAKE: bool = false;
    if POLL_SAFETY_WAKE && ticks() % 10 == 0 { sched::try_wake_poll(); }
}

/// Shared K2 block sequence for poll/ppoll/select: park on the global poll
/// wait-channel until an edge (`wake_poll`), a deliverable signal, or the
/// deadline tick. `reprobe` is a read-only "is anything ready now" check — it
/// must not write user memory (that happens at the caller's loop top). The
/// re-probe between prepare and commit closes the check-then-sleep lost-wake.
///
/// `set` is `None` when the call names no fd at all: `poll(NULL, 0, ms)` (the
/// classic portable sleep), a pollfd array whose every fd is negative, or a
/// `select` with no read/write bits (only exceptfds, which never fire here).
/// Nothing can become ready then, so the park waits for its deadline and
/// signals only (`POLL_MASK_TIMED_ONLY`, like nanosleep: a signal readies the
/// thread as it is posted). It used to park broadcast, so every poll wake in
/// the system woke a `poll(NULL, 0, 100)` sleeper to re-check its clock.
fn poll_block(infinite: bool, deadline: u64, set: Option<sched::PollSet>, gen0: u64, reprobe: impl FnOnce() -> bool) {
    let deadline = if infinite { u64::MAX } else { deadline };
    match set {
        // An interest set that collected no bits (untaggable) parks broadcast
        // inside `block_on_poll_prepare_set`, so it is never immune to a wake.
        Some(set) => sched::block_on_poll_prepare_set(deadline, &set),
        None => sched::block_on_poll_prepare_masked(deadline, sched::POLL_MASK_TIMED_ONLY),
    }
    if poll_gen() != gen0
        || reprobe() || interrupted() || (!infinite && monotonic_ns() >= deadline) {
        sched::block_on_poll_cancel();
        return;
    }
    sched::block_on_poll_commit();
}

/// The targeted-wake mask for a directly-watched fd (poll/ppoll/select). One
/// VFS_POLL/NET_POLL round trip via `probe_fd_events_seq`; the console tag
/// for the console shortcuts, broadcast for any untagged server.
fn poll_fd_tag(pid: u32, fd: usize) -> sched::PollTag {
    probe_fd_events_seq(pid, fd, u32::MAX).2
}

/// Add one interest's probe answer to a park's `PollSet`. A nested epoll fd is
/// expanded into its own interests' tags (keys included) rather than added as
/// the keyless OR `epoll_tag_mask` reports for it: event loops that nest an
/// epoll set (calloop inside tokio, a GLib/poll() wrapper around epoll) would
/// otherwise lose every key of the inner set, and a keyless bit is never
/// refined (see `sched::PollSet`).
fn poll_set_add(set: &mut sched::PollSet, pid: u32, tag: &sched::PollTag, depth: u32) {
    if tag.nested != 0 {
        epoll_collect(pid, tag.nested as usize - 1, depth + 1, set);
    } else {
        set.add(tag);
    }
}

/// `epoll_tag_mask` into a `PollSet`, keeping the inner interests' keys.
fn epoll_collect(pid: u32, slot: usize, depth: u32, set: &mut sched::PollSet) {
    if depth >= EPOLL_MAX_NEST { set.add_bits(sched::POLL_TAG_ALL); return; }
    set.add_key(epoll_self_key(slot)); // epoll_ctl wakes it by this key
    let hi = { EPOLL_INSTANCES.lock()[slot].hi as usize };
    for i in 0..hi {
        let interest = { EPOLL_INSTANCES.lock()[slot].interests[i] };
        if !interest.in_use || !interest.armed { continue; }
        sched::wakestat::EP_INNER.fetch_add(1, Ordering::Relaxed);
        let (_cur, _seq, tag) = probe_interest(pid, slot, &interest, depth + 1);
        poll_set_add(set, pid, &tag, depth);
    }
}

/// OR of the tags of every fd a `poll`/`ppoll` set names. Run only on the park
/// path (nready == 0), so the extra round trips cost nothing while events are
/// ready. Negative (skipped) pollfd entries contribute nothing. `None` when
/// no entry names an fd (a timed sleep, see `poll_block`); an untaggable set
/// (no bits) is turned into broadcast by `poll_block`.
fn poll_fds_mask(pid: u32, fds_ptr: usize, nfds: usize) -> Option<sched::PollSet> {
    let mut mask = sched::PollSet::new();
    let mut named = false;
    for i in 0..nfds {
        let pfd = fds_ptr + i * 8;
        let fd = ur::<i32>(pfd);
        if fd < 0 { continue; }
        named = true;
        poll_set_add(&mut mask, pid, &poll_fd_tag(pid, fd as usize), 0);
    }
    if named { Some(mask) } else { None }
}

/// OR of the tags of every fd a `select` set names (read or write side).
/// `None` when it names none (`select(0, ...)`, exceptfds only): see
/// `poll_block`.
fn select_fds_mask(pid: u32, nfds: usize, rfds: usize, wfds: usize,
                    has_r: bool, has_w: bool) -> Option<sched::PollSet> {
    let mut mask = sched::PollSet::new();
    let mut named = false;
    for fd in 0..nfds {
        let want_r = has_r && user_fdset_bit(rfds, fd);
        let want_w = has_w && user_fdset_bit(wfds, fd);
        if !want_r && !want_w { continue; }
        named = true;
        poll_set_add(&mut mask, pid, &poll_fd_tag(pid, fd), 0);
    }
    if named { Some(mask) } else { None }
}

/// Read-only "any pollfd ready" scan for the poll/ppoll re-probe (no revents
/// written back — the caller's loop re-scans and writes on the next pass).
fn poll_any_ready(pid: u32, fds_ptr: usize, nfds: usize) -> bool {
    for i in 0..nfds {
        let pfd = fds_ptr + i * 8;
        let fd     = ur::<i32>(pfd      );
        let events = ur::<i16>(pfd + 4);
        if fd < 0 { continue; }
        if vfs::is_handle(fd as usize) { continue; }
        let revents = probe_fd_events(pid, fd as usize, events as u16 as u32);
        if revents != 0 { return true; }
    }
    false
}

/// Read-only "any fd ready" scan for the select re-probe.
fn select_any_ready(pid: u32, nfds: usize, rfds: usize, wfds: usize,
                    has_r: bool, has_w: bool) -> bool {
    const POLLIN:  u32 = 0x0001;
    const POLLOUT: u32 = 0x0004;
    for fd in 0..nfds {
        let want_r = has_r && user_fdset_bit(rfds, fd);
        let want_w = has_w && user_fdset_bit(wfds, fd);
        if !want_r && !want_w { continue; }
        let requested = (if want_r { POLLIN } else { 0 }) | (if want_w { POLLOUT } else { 0 });
        let ev = probe_fd_events(pid, fd, requested);
        if (want_r && ev & POLLIN != 0) || (want_w && ev & POLLOUT != 0) { return true; }
    }
    false
}

/// Query real, current readiness for `fd` — routes to the owning server
/// (VFS for fd < `net_server::SOCK_FD_BASE`, net otherwise) or, for fd 0-2,
/// the same evdev/serial/always-writable checks the console already used.
/// Returns the fd's true POLLIN/POLLOUT/POLLERR/POLLHUP/POLLNVAL state.
fn poll_fd_state(pid: u32, fd: usize) -> u32 { poll_fd_state_nested(pid, fd, 0) }

/// `poll_fd_state` with the nested-epoll recursion depth threaded through.
/// `depth` is 0 for a poll/select/epoll_wait interest named directly by
/// userspace, and one higher for each epoll fd traversed to reach it.
fn poll_fd_state_nested(pid: u32, fd: usize, depth: u32) -> u32 {
    // An ordinary descriptor: one fd-table pass (`vfs::poll_fd_direct`).
    if let Some((ev, _, _)) = vfs::poll_fd_direct(pid, fd, 0) { return ev; }
    let fd = vfs::resolve_fd(pid, fd);
    const POLLIN:   u32 = 0x0001;
    const POLLOUT:  u32 = 0x0004;
    const POLLNVAL: u32 = 0x0020;

    // A pty end on 0/1/2 — a shell's stdio under a terminal emulator is
    // exactly this — is a terminal, but not the console. Its readiness lives
    // in the pty pool, so it must fall through to VFS_POLL rather than be
    // answered from the serial port's state. Same rule sys_read/sys_write
    // already follow via fd_redirected; checked here because the two shortcuts
    // below are unconditional.
    //
    // `/dev/tty0` dup2'd onto stdio is excluded for the same reason a pty is:
    // its readiness is the VT-change edge the VFS answers, not the console's.
    //
    // Only a descriptor that IS the console takes the shortcuts: a regular
    // file, /dev/null or anything else without a handle of its own keeps its
    // number through resolve_fd, so `sh < file` used to poll the serial port
    // for the file's readiness, and a shell `read` that polls fd 0 first
    // blocked forever on a redirected stdin.
    let console0 = fd <= 2 && vfs::fd_is_console_stdio(pid, fd);
    let kind0 = if console0 { vfs::vfs_get_node_kind(pid, fd) } else { None };
    let is_vt0 = matches!(kind0, Some(vfs::VnodeKind::DevVt { vt: 0, .. }));
    if fd == 0 && console0 && !is_vt0 {
        return if console_input_pending()
            || evdev_server::has_key_event(0) || crate::serial_has_data() { POLLIN } else { 0 };
    }
    if (fd == 1 || fd == 2) && console0 && !is_vt0 {
        return POLLOUT;
    }
    // Console proxy fds (/dev/tty, a dup'd stdin) mirror fd 0/1/2 readiness —
    // crossterm registers its /dev/tty handle in epoll and waits on it for
    // the cursor-position reply; VFS_POLL reports DevStdio not-ready, which
    // hung that wait. `/dev/tty0` is the exception: its readiness is the
    // VT-change edge the VFS answers, not console input.
    if fd < net_server::SOCK_FD_BASE && vfs::fd_is_console_stdio(pid, fd)
        && !is_vt0 && vfs::fd_vt_number(pid, fd) != Some(0) {
        return if console_input_pending()
            || evdev_server::has_key_event(0) || crate::serial_has_data() {
            POLLIN | POLLOUT
        } else {
            POLLOUT
        };
    }
    // A nested epoll fd is itself pollable, and Linux reports it readable
    // exactly when its OWN interest list has at least one ready event (never
    // writable). Without this case the fd falls through to the socket branch
    // below — EPOLL_FD_BASE (0x400) is above SOCK_FD_BASE (0x200) — NET_POLL
    // rejects it as not-a-socket, and probe_fd_events passes the resulting
    // POLLNVAL through unconditionally, so the interest fires on EVERY pass.
    // Any event loop that nests one reactor inside another then busy-spins
    // with nothing to do: calloop wrapping wayland-backend's own epoll (its
    // `poll_fd()` IS an epoll fd) returned from `dispatch(100ms)` ~4600×/s in
    // the COSMIC panel.
    if is_epoll_handle(fd) {
        let slot = match epoll_slot_for(fd, sched::tgid_of(pid)) { Some(s) => s, None => return POLLNVAL };
        {
            // Scoped: the lock must be dropped before the recursive probe —
            // epoll_any_ready_nested re-takes it per interest (invariant
            // 82d0cc3: never hold a spinlock across a server call).
            let ep = EPOLL_INSTANCES.lock();
            if !ep[slot].in_use { return POLLNVAL; }
        }
        // Depth-bounded like Linux's EP_MAX_NESTS: an epoll instance that
        // (transitively) watches itself would otherwise recurse forever.
        // Reporting not-ready at the limit can only delay an event, never spin.
        if depth >= EPOLL_MAX_NEST { return 0; }
        return if epoll_any_ready_nested(pid, slot, depth + 1) { POLLIN } else { 0 };
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
    probe_fd_events_nested(pid, fd, requested, 0)
}

/// `probe_fd_events` with the nested-epoll recursion depth threaded through.
fn probe_fd_events_nested(pid: u32, fd: usize, requested: u32, depth: u32) -> u32 {
    const POLLERR:  u32 = 0x0008;
    const POLLHUP:  u32 = 0x0010;
    const POLLNVAL: u32 = 0x0020;
    let state = poll_fd_state_nested(pid, fd, depth);
    (state & requested) | (state & (POLLERR | POLLHUP | POLLNVAL))
}

/// Like `probe_fd_events`, but also returns the fd's edge-trigger event-seq
/// so `sys_epoll_wait` can emulate EPOLLET without dropping edges. `Some(seq)`
/// is a monotonic per-object counter for VFS fds (pipes/eventfd/timerfd — see
/// VFS handle_poll); `None` means the fd has no edge source (net sockets, fd
/// 0-2), so the caller must treat it level-triggered. The revents masking
/// matches `probe_fd_events` exactly.
fn probe_fd_events_seq(pid: u32, fd: usize, requested: u32) -> (u32, Option<u64>, sched::PollTag) {
    probe_fd_events_seq_nested(pid, fd, requested, 0)
}

/// `probe_fd_events_seq` with the nested-epoll recursion depth threaded through.
///
/// The third return value is the targeted-wake tag for this interest (see
/// `sched::poll_tag`): the caller ORs it into the parked poller's `poll_mask`.
/// It is `POLL_TAG_ALL` (broadcast) for anything with no per-object tag (a
/// device server that does not fill data[32]) so a
/// forgotten producer degrades to today's herd, never to a lost wake.
fn probe_fd_events_seq_nested(pid: u32, fd: usize, requested: u32, depth: u32)
    -> (u32, Option<u64>, sched::PollTag)
{
    // want_ofd 0 never answers None.
    probe_fd_events_seq_inner(pid, fd, requested, depth, 0).unwrap_or((POLLNVAL_EP, None, sched::PollTag::ALL))
}

/// The body of `probe_fd_events_seq_nested`. `want_ofd` != 0 asks for `fd`
/// only while it names that open file description (`vfs::ofd`): `None` when
/// it is closed or names another one (see `probe_interest`).
fn probe_fd_events_seq_inner(pid: u32, fd: usize, requested: u32, depth: u32, want_ofd: u32)
    -> Option<(u32, Option<u64>, sched::PollTag)>
{
    const POLLERR_D:  u32 = 0x0008;
    const POLLHUP_D:  u32 = 0x0010;
    const POLLNVAL_D: u32 = 0x0020;
    // An ordinary descriptor: one fd-table pass (`vfs::poll_fd_direct`),
    // answered as the VFS_POLL arm at the bottom would.
    if let Some((state, seq, tag)) = vfs::poll_fd_direct(pid, fd, want_ofd) {
        let masked = (state & requested) | (state & (POLLERR_D | POLLHUP_D | POLLNVAL_D));
        return Some((masked, Some(seq), tag));
    }
    let fd = vfs::resolve_fd(pid, fd);
    const POLLERR:  u32 = 0x0008;
    const POLLHUP:  u32 = 0x0010;
    const POLLNVAL: u32 = 0x0020;

    // A nested epoll fd has no edge source of its own, so it stays
    // level-triggered (None) — its readiness is recomputed from its interest
    // list on every probe. Checked before the console/socket routing below
    // because EPOLL_FD_BASE sits inside the "not a VFS fd" range. Its tag is
    // its own tag OR its children's (see `epoll_tag_mask`); an empty nested
    // set contributes just its own tag, which a later add wakes.
    if is_epoll_handle(fd) {
        let state = probe_fd_events_nested(pid, fd, requested, depth);
        let tag = match epoll_slot_of(fd) {
            Some(slot) => sched::PollTag { nested: slot as u32 + 1,
                                           ..sched::PollTag::bits(epoll_tag_mask(pid, slot, depth)) },
            None       => sched::PollTag::ALL,
        };
        return Some((state, None, tag));
    }
    // fd 0-2 and console stdio proxies (/dev/tty, dup'd stdin — VFS DevStdio
    // vnodes) have no edge source and stay level-triggered (None): VFS
    // handle_poll reports DevStdio never-ready, so routing them to VFS_POLL
    // below would leave an epoll interest that can never fire (crossterm's
    // mio-registered /dev/tty handle waits there for the ESC[6n reply).
    // A pty on 0/1/2 is excluded: it DOES have an edge source (the pool's per
    // pair seq), and routing it level-triggered here would make an EPOLLET
    // reader on a pty spin on every epoll_wait return.
    // Scoped to fd<=2 so the common case costs no extra FD_TABLES lock in
    // what is a per-interest, per-epoll_wait-pass hot path.
    // `/dev/tty0` is excluded for the opposite reason: it DOES have an edge
    // source (the active VT number — see vfs::fd_vt_number), and routing it
    // level-triggered here would make an EPOLLET libseat shim re-fire on every
    // epoll_wait return for a switch it has already consumed. The fd<=2 case is
    // answered from the node kind already fetched for the pty test rather than
    // a second FD_TABLES pass, and the fd>2 case only from fds
    // fd_is_console_stdio has already accepted — neither adds a lookup to the
    // per-interest, per-epoll_wait-pass hot path.
    // Like poll_fd_state: fd 0-2 only while they name the console (a file
    // redirected onto stdin goes to VFS_POLL, which reports it always ready).
    let console0 = fd <= 2 && vfs::fd_is_console_stdio(pid, fd);
    let kind0 = if console0 { vfs::vfs_get_node_kind(pid, fd) } else { None };
    let is_vt0 = matches!(kind0, Some(vfs::VnodeKind::DevVt { vt: 0, .. }));
    if (console0 && !is_vt0)
        || (fd > 2 && vfs::fd_is_console_stdio(pid, fd)
                   && vfs::fd_vt_number(pid, fd) != Some(0))
    {
        // fd 0-2 / console proxies: the console's own tag (`console_poll_tag`),
        // the same one a blocking console read parks on. fd 1/2 of the console
        // answer a constant POLLOUT (`poll_fd_state`), so they get the static
        // tag; everything else here reports console input.
        if want_ofd != 0 && vfs::fd_ofd(pid, fd) != Some(want_ofd) { return None; }
        let output_only = (fd == 1 || fd == 2) && console0 && !is_vt0;
        return Some((probe_fd_events(pid, fd, requested), None, console_poll_tag(!output_only)));
    }
    // Net sockets: a connected AF_UNIX socket and every inet socket (TCP,
    // UDP, ICMP, TCP listeners) carry an edge-seq (data[16]==1) so an
    // EPOLLET tokio socket is edge-gated instead of re-firing on every
    // level-writable epoll_wait return (a TCP one made sshd's reactor spin
    // at 100% per session); AF_UNIX listeners report no seq (data[16]==0) →
    // level. data[32]==1 carries the poll tag (AF_UNIX, connected TCP/UDP);
    // the rest leave it 0 → broadcast.
    if fd >= net_server::SOCK_FD_BASE {
        // `requested` narrows a connected AF_UNIX end's tag to the directions
        // asked for (see net_server's unix_end_tag); u32::MAX / 0 = both.
        let msg = make_vfs_msg(net_server::NET_POLL, &[fd as u64, requested as u64, want_ofd as u64]);
        let reply = net_server::handle(&msg, pid);
        let r = net_reply_val(&reply);
        if want_ofd != 0 && (r == -116 || r == -9) { return None; }
        let state = if r < 0 { POLLNVAL } else { r as u32 };
        let seq = if reply.data[16] == 1 {
            Some(u64::from_le_bytes(reply.data[8..16].try_into().unwrap_or([0u8; 8])))
        } else { None };
        let tag = reply_poll_tag(&reply);
        let masked = (state & requested) | (state & (POLLERR | POLLHUP | POLLNVAL));
        return Some((masked, seq, tag));
    }
    let msg = make_vfs_msg(vfs::VFS_POLL, &[fd as u64, want_ofd as u64]);
    let reply = vfs::handle(&msg, pid);
    let r = vfs_reply_val(&reply);
    if want_ofd != 0 && (r == -116 || r == -9) { return None; }
    let state = if r < 0 { POLLNVAL } else { r as u32 };
    let seq = u64::from_le_bytes(reply.data[8..16].try_into().unwrap_or([0u8; 8]));
    let tag = reply_poll_tag(&reply);
    let masked = (state & requested) | (state & (POLLERR | POLLHUP | POLLNVAL));
    Some((masked, Some(seq), tag))
}

/// Read the targeted-wake tag from a VFS_POLL / NET_POLL reply: data[24..32]
/// when the valid flag data[32]==1, else `POLL_TAG_ALL` (broadcast). A server
/// that has not been converted leaves data[32]==0 and its pollers degrade to
/// broadcast — never to tag 0, which would contribute nothing and lose a wake.
fn reply_poll_tag(reply: &Message) -> sched::PollTag {
    if reply.data[32] != 1 { return sched::PollTag::ALL; }
    let bits = u64::from_le_bytes(reply.data[24..32].try_into().unwrap_or([0u8; 8]));
    // Exact keys (see `sched::PollSet`): marker data[33] == POLL_REPLY_KEYS,
    // count data[34] (<= 3), keys at data[40..64]. `PollSet::add` ignores a key
    // whose bit is not in `bits`, so a stale/foreign byte here can never
    // narrow anything.
    let mut t = sched::PollTag::bits(bits);
    if reply.data[33] == POLL_REPLY_KEYS {
        let n = (reply.data[34] as usize).min(3);
        for i in 0..n {
            let o = 40 + i * 8;
            t.keys[i] = u64::from_le_bytes(reply.data[o..o + 8].try_into().unwrap_or([0u8; 8]));
        }
        t.n = n as u8;
    } else if reply.data[33] == sched::POLL_REPLY_FOREIGN && bits != sched::POLL_TAG_ALL {
        // The server vouches that no keyed wake ever targets this object
        // (see `sched::PollTag::foreign`).
        t.foreign = true;
    }
    t
}

/// data[33] marker of a VFS_POLL / NET_POLL reply that carries exact keys
/// (see `reply_poll_tag`); shared with the servers.
const POLL_REPLY_KEYS: u8 = sched::POLL_REPLY_KEYS;

/// OR of the tags of an epoll instance's armed interests — the tag a nested or
/// directly-watched epoll fd contributes. Recurses through nested epoll fds;
/// at the nesting limit it cannot enumerate, so it returns `POLL_TAG_ALL`
/// (broadcast) rather than narrowing. An empty instance returns 0 (contributes
/// nothing — a later epoll_ctl ADD broadcasts). Per-interest EPOLL_INSTANCES
/// lock, dropped before each probe (invariant 82d0cc3), mirroring
/// `epoll_any_ready_nested`.
fn epoll_tag_mask(pid: u32, slot: usize, depth: u32) -> u64 {
    if depth >= EPOLL_MAX_NEST { return sched::POLL_TAG_ALL; }
    let hi = { EPOLL_INSTANCES.lock()[slot].hi as usize };
    // The instance's own tag first: an epoll_ctl on it must reach whoever
    // watches it (see sys_epoll_ctl).
    let mut mask = epoll_self_tag(slot);
    for i in 0..hi {
        let interest = { EPOLL_INSTANCES.lock()[slot].interests[i] };
        if !interest.in_use || !interest.armed { continue; }
        sched::wakestat::EP_INNER.fetch_add(1, Ordering::Relaxed);
        let (_cur, _seq, tag) = probe_interest(pid, slot, &interest, depth + 1);
        if CHURN_STATS && tag.bits == sched::POLL_TAG_ALL { churn_all_tag_note(pid, interest.fd as usize); }
        mask |= tag.bits;
    }
    mask
}

/// The tag `epoll_ctl` on instance `slot` wakes (see sys_epoll_ctl).
fn epoll_self_tag(slot: usize) -> u64 {
    sched::poll_tag(sched::poll_class::EPOLL, slot as u32)
}

/// The key of `epoll_self_tag` (epoll_ctl's wake and widening).
fn epoll_self_key(slot: usize) -> u64 {
    sched::poll_key(sched::poll_class::EPOLL, slot as u32)
}

/// `[CHURN] alltag` census: which interest kinds make an epoll wait mask a
/// broadcast one (index: 0 inet/listener socket, 1 epoll nest, 2 signalfd,
/// 3 inotify, 4 device, 5 stdio/none, 6 file, 7 other).
static ALLTAG: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
fn churn_all_tag_note(pid: u32, fd: usize) {
    let k = if fd >= net_server::SOCK_FD_BASE && fd < EPOLL_FD_BASE { 0 }
        else if fd >= EPOLL_FD_BASE { 1 }
        else {
            match vfs::vfs_get_node_kind(pid, fd) {
                Some(vfs::VnodeKind::SignalFd { .. }) => 2,
                Some(vfs::VnodeKind::Inotify { .. }) => 3,
                Some(vfs::VnodeKind::DynamicDevice { .. }) => 4,
                Some(vfs::VnodeKind::DevStdio { .. }) | Some(vfs::VnodeKind::None) | None => 5,
                Some(vfs::VnodeKind::MountedFile { .. }) | Some(vfs::VnodeKind::TmpFile { .. })
                    | Some(vfs::VnodeKind::RamFile { .. }) => 6,
                _ => 7,
            }
        };
    ALLTAG[k].fetch_add(1, Ordering::Relaxed);
}

fn sys_eventfd2(initval: usize, flags: usize, frame_ptr: usize) -> isize {
    let pid = current_pid();
    // EFD_NONBLOCK/EFD_CLOEXEC (== O_NONBLOCK/O_CLOEXEC) must be recorded — see
    // sys_timerfd_create for why a dropped O_NONBLOCK wedges an event loop.
    let msg = make_vfs_msg(vfs::VFS_EVENTFD, &[initval as u64, flags as u64]);
    let fd = vfs_reply_val(&vfs::handle(&msg, pid));
    if fd >= 0 { vfs::eventfd_note(pid, fd as usize, vfs::EVFD_ORIGIN_EVENTFD2, 0, user_backtrace(frame_ptr)); }
    fd
}

/// Capture user backtraces for fd/handle provenance (eventfd2, the virtgpu
/// out-fence eventfd, PRIME imports) and print them on Ctrl-T (`[EVFD]`,
/// `[PRIME]`). Off by default: it walks up to 16 user frames per EXECBUFFER.
/// This is how lane compfdleak (2026-09-27) traced cosmic-comp's retained fence
/// eventfds to smithay `GlesFrame::render_texture_from_to` (a GlesTexture
/// read_sync) and the retained import to `GlesRenderer::import_dmabuf` of a
/// closed window's last buffer. Symbolize a PIE main image at 0x20_0000
/// (`MAIN_DYN_BASE`), e.g. `eu-addr2line -e cosmic-comp $((pc - 0x200000))`.
const FD_PROVENANCE_BT: bool = false;

/// The caller's user pc plus up to fifteen return addresses from its frame-
/// pointer chain (aarch64 x29 records, x86_64 rbp records). Diagnostic only;
/// all-zero unless `FD_PROVENANCE_BT`. Unreadable frames just end the list.
fn user_backtrace(frame_ptr: usize) -> [u64; 16] {
    let mut bt = [0u64; 16];
    if !FD_PROVENANCE_BT || frame_ptr == 0 { return bt; }
    let uf = unsafe { &*(frame_ptr as *const sched::context::UserFrame) };
    #[cfg(target_arch = "aarch64")]
    let (pc, lr, mut fp) = (uf.elr_el1, uf.x[30], uf.x[29] as usize);
    #[cfg(target_arch = "x86_64")]
    let (pc, lr, mut fp) = (uf.rcx, 0u64, uf.rbp as usize);
    bt[0] = pc;
    let mut n = 1;
    if lr != 0 { bt[1] = lr; n = 2; }
    while n < 16 {
        if fp == 0 || fp % 8 != 0 { break; }
        let mut rec = [0u8; 16];
        if !with_current_address_space(|as_| as_.read_user_buf(fp, &mut rec)).unwrap_or(false) { break; }
        let prev = usize::from_ne_bytes(rec[0..8].try_into().unwrap());
        let ret = u64::from_ne_bytes(rec[8..16].try_into().unwrap());
        if ret != bt[n - 1] { bt[n] = ret; n += 1; }
        if prev <= fp { break; }
        fp = prev;
    }
    bt
}

/// signalfd4(ufd, mask, sizemask, flags) — minimal: a pseudo-fd that reports
/// POLLIN when a masked signal is pending and, when read, dequeues each pending
/// masked signal as a 128-byte signalfd_siginfo with ssi_signo populated (the
/// rest zero — calloop reads only ssi_signo). `ufd == -1` creates a new
/// signalfd; otherwise the named signalfd's mask is replaced.
fn sys_signalfd4(ufd: usize, mask_ptr: usize, _sizemask: usize, flags: usize) -> isize {
    if mask_ptr == 0 || !validate_user_buf(mask_ptr, 8) { return -14; } // EFAULT
    let mask = ur::<u64>(mask_ptr);
    let existing = if ufd as isize == -1 { usize::MAX } else { ufd };
    let pid = current_pid();
    // SFD_NONBLOCK/SFD_CLOEXEC (== O_NONBLOCK/O_CLOEXEC) must be recorded on a
    // newly-created signalfd — see sys_timerfd_create.
    let msg = make_vfs_msg(vfs::VFS_SIGNALFD_CREATE, &[existing as u64, mask, flags as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// inotify_init1(flags) — a valid fd that accepts watches but never delivers
/// events (keeps a config-watch source quiet in an event loop; no live reload).
fn sys_inotify_init1(flags: usize) -> isize {
    let pid = current_pid();
    // Forward IN_CLOEXEC/IN_NONBLOCK so the fd is close-on-exec when requested;
    // dropping IN_CLOEXEC leaked every config-watch inotify fd across execve and
    // filled late children's fd tables (cosmic-greeter EMFILE crash-loop).
    let msg = make_vfs_msg(vfs::VFS_INOTIFY_CREATE, &[flags as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// inotify_add_watch(fd, path, mask) → a fake watch descriptor (≥1). The path
/// and mask are accepted and discarded — the watch never fires.
fn sys_inotify_add_watch(fd: usize, _path_ptr: usize, _mask: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_INOTIFY_ADD, &[fd as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// inotify_rm_watch(fd, wd) → 0 (nothing to remove; watches never fired).
fn sys_inotify_rm_watch(_fd: usize, _wd: usize) -> isize { 0 }

/// memfd_create(name_ptr, flags) → writable anonymous fd backed by a TmpFile.
fn sys_memfd_create(name_ptr: usize, _flags: usize) -> isize {
    // A memfd must be a DISTINCT anonymous inode every call, even when two calls
    // pass identical names (Linux semantics — the name is only a debug label).
    // We back memfds with named tmpfs nodes "/tmp/memfd:<name>", so a reused name
    // would otherwise reopen the SAME inode: smithay-client-toolkit creates every
    // wl_shm SlotPool with the one fixed name "smithay-client-toolkit", and once
    // the first pool sealed its inode (F_SEAL_SHRINK|F_SEAL_SEAL), the next
    // memfd_create's O_TRUNC shrank a sealed file → EPERM, which panicked every
    // libcosmic/winit client (cosmic-panel, cosmic-notifications). Create with
    // O_EXCL and, on EEXIST, append a monotonic ":<seq>" suffix until the inode
    // is unique. The suffix is skipped on the first try so the common
    // unique-name case keeps its clean "/tmp/memfd:<name>" path.
    static MEMFD_SEQ: AtomicUsize = AtomicUsize::new(1);
    const O_RDWR:  u64 = 0x2;
    const O_CREAT: u64 = 0x40; // 0o100
    const O_EXCL:  u64 = 0x80;
    const EEXIST:  isize = -17;
    let pid = current_pid();
    // The name is copied once, up front, through the fault-tolerant string
    // copy: Linux's limit (249 bytes, EINVAL beyond) and its EFAULT for NULL
    // or an unreadable name. It used to be read a byte at a time straight
    // from the user pointer, so a bad one killed the caller. Only a prefix
    // goes into the node name, and a '/' in it is not a directory.
    const MFD_NAME_MAX: usize = 249;
    let name = match copy_user_str(name_ptr, MFD_NAME_MAX + 1) {
        Ok(v) => v,
        Err(-36) => return -22, // EINVAL: name too long
        Err(e) => return e,
    };
    let mut suffix: usize = 0; // 0 => no suffix
    loop {
        let mut path = [0u8; 80];
        let prefix = b"/tmp/memfd:";
        path[..prefix.len()].copy_from_slice(prefix);
        let mut plen = prefix.len();
        for &b in name.iter().take(48) {
            if plen < 60 { path[plen] = if b == b'/' { b'_' } else { b }; plen += 1; }
        }
        if name.is_empty() && plen < 60 {
            path[plen] = b'0'; plen += 1;
        }
        if suffix != 0 {
            if plen < 62 { path[plen] = b':'; plen += 1; }
            let mut digits = [0u8; 20]; let mut d = 0; let mut v = suffix;
            while v > 0 && d < 20 { digits[d] = b'0' + (v % 10) as u8; d += 1; v /= 10; }
            for i in (0..d).rev() { if plen < 79 { path[plen] = digits[i]; plen += 1; } }
        }
        let _ = plen;
        let msg = make_vfs_msg(vfs::VFS_OPEN, &[
            path.as_ptr() as u64,
            O_RDWR | O_CREAT | O_EXCL, // fresh inode; O_EXCL guarantees empty, so
                                       // no O_TRUNC (which a sealed reopen would
                                       // reject). O_RDWR keeps the wl_shm
                                       // read()-coherence path legal (K1).
            0o600u64,
        ]);
        let fd = vfs_reply_val(&vfs::handle(&msg, pid));
        if fd == EEXIST {
            suffix = MEMFD_SEQ.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if fd >= 0 {
            // Mark the inode as a memfd: creates an empty seal-capable VMO so the
            // file is genuinely shareable and F_ADD_SEALS/F_GET_SEALS are honored.
            vfs::mark_memfd(pid, fd as usize);
            // [GAP2] Name the memfd, its owner pid and its tmpfs slot, so every
            // later [G2*] line can be attributed to a process and a pool. Also
            // arms the [G2SUM] content sampler on the applet's pool.
            if mm::gap2::ON {
                let g2_idx = vfs::gap2_tmpfile_idx(pid, fd as usize)
                    .unwrap_or(usize::MAX);
                mm::gap2::s("[G2MEMFD] pid="); mm::gap2::h(pid as usize);
                mm::gap2::kv(" fd=",  fd as usize);
                mm::gap2::kv(" idx=", g2_idx);
                mm::gap2::s(" name="); mm::gap2::bytes(&path[..plen]);
                mm::gap2::nl();
                if g2_idx != usize::MAX
                    && path[..plen].starts_with(b"/tmp/memfd:leandros-applet")
                {
                    mm::gap2::set_watch_primary(g2_idx);
                }
            }
            // Drop the name immediately, exactly as the PRIME export node does
            // (36f62d0): a memfd is an ANONYMOUS inode that dies with its last
            // fd. The open fd holds the slot — tmp_drop_name sees it in
            // tmp_open_fd_mask and marks the inode `ephemeral` rather than
            // freeing it, and tmp_release_ephemeral collects it (and its VMO
            // frames) on the final close. Without this every call burnt one of
            // the 128 MAX_TMP_FILES slots forever and a client that allocates a
            // wl_shm pool per frame bricked memfd_create system-wide (ENOSPC)
            // after ~100 frames, across /tmp, /dev/shm and /run/user alike.
            //
            // The comment this replaces claimed the fd's later ftruncate/mmap
            // still resolved the inode BY NAME and so would break. They do not:
            // every fd-side operation keys off VnodeKind::TmpFile { idx } —
            // handle_ftruncate, vmo_acquire_frames (via tmpfile_owner_of),
            // read/write/lseek/fstat/fchmod/fchown, F_ADD_SEALS/F_GET_SEALS and
            // the f*xattr family. The only by-name site is handle_open, and
            // nothing ever reopens a memfd by name (tmp_find skips `ephemeral`,
            // which is precisely the invisibility Linux gives a memfd). What
            // that comment was really seeing was the ENOMEM from
            // tmpfile_owner_of missing a non-leader thread's fd table — fixed
            // in servers/vfs alongside this.
            let unlink_msg = make_vfs_msg(vfs::VFS_UNLINK, &[path.as_ptr() as u64]);
            let _ = vfs::handle(&unlink_msg, pid);
        }
        return fd;
    }
}

fn sys_timerfd_create(clockid: usize, flags: usize) -> isize {
    // CLOCK_REALTIME or CLOCK_MONOTONIC (Linux also takes BOOTTIME and the
    // _ALARM variants; nothing here suspends, so BOOTTIME is MONOTONIC).
    let clockid = match clockid {
        CLOCK_REALTIME => CLOCK_REALTIME,
        CLOCK_MONOTONIC | CLOCK_BOOTTIME => CLOCK_MONOTONIC,
        _ => return -22, // EINVAL
    };
    let pid = current_pid();
    // flags carries TFD_NONBLOCK/TFD_CLOEXEC (== O_NONBLOCK/O_CLOEXEC). It MUST
    // be recorded on the fd: the `polling` crate (calloop's poller) creates its
    // timeout timerfd with TFD_NONBLOCK and reads it expecting EAGAIN when it
    // hasn't fired. If the flag is dropped, fd_nonblock() is false and the
    // kernel read path yield-spins on EAGAIN, pinning the caller's thread in the
    // read instead of letting its event loop poll other fds — which is exactly
    // how a single-threaded compositor stops accepting wayland clients.
    let msg = make_vfs_msg(vfs::VFS_TIMERFD_CREATE, &[flags as u64, clockid as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// `TFD_TIMER_ABSTIME` — timerfd_settime's absolute-deadline mode.
const TFD_TIMER_ABSTIME: usize = 1;
/// `TFD_TIMER_CANCEL_ON_SET` — asks a CLOCK_REALTIME absolute timer to fail
/// with ECANCELED if the wall clock is stepped under it: the fd polls
/// readable and its next read() answers ECANCELED (see
/// `vfs::timerfd_clock_was_set`, run by `clock_settime`/`settimeofday`).
/// Ignored, as on Linux, without TFD_TIMER_ABSTIME or on CLOCK_MONOTONIC.
const TFD_TIMER_CANCEL_ON_SET: usize = 2;

/// timerfd_settime(fd, flags, new_value_ptr, old_value_ptr)
///
/// Reads itimerspec {interval, value} from new_value_ptr (2×16 bytes) and
/// hands the vfs `it_value` plus whether it is absolute; the vfs, which knows
/// the fd's clockid, turns it into an absolute deadline on the monotonic
/// clock (see `handle_timerfd_settime`):
///
/// * with `TFD_TIMER_ABSTIME`, `it_value` is an instant on the fd's clock —
///   CLOCK_REALTIME is translated through the wall-clock offset. This used to
///   be ignored and the value armed as a relative interval, firing at
///   `uptime + deadline` — the same landmine `sys_clock_nanosleep`'s
///   TIMER_ABSTIME and FUTEX_WAIT_BITSET had;
/// * without it, `it_value` is added to `monotonic_ns()` NOW — not to the
///   tick counter, which ignores the fraction of the tick already elapsed and
///   fired up to 10 ms early by clock_gettime's reckoning. Linux converts
///   relative to absolute at settime for the same reason.
///
/// `old_value_ptr`, if given, receives the timer's previous setting exactly as
/// timerfd_gettime would report it, before the new one is armed.
fn sys_timerfd_settime(fd: usize, flags: usize, new_ptr: usize, old_ptr: usize) -> isize {
    if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 { return -22; } // EINVAL
    if new_ptr == 0 || !validate_user_buf(new_ptr, 32) { return -14; } // EFAULT
    let (value_ns, interval_ns) = unsafe {
        let iv_sec  = ur::<i64>(new_ptr);      // interval.tv_sec
        let iv_nsec = ur::<i64>(new_ptr + 8);  // interval.tv_nsec
        let vl_sec  = ur::<i64>(new_ptr + 16); // value.tv_sec
        let vl_nsec = ur::<i64>(new_ptr + 24); // value.tv_nsec
        if iv_sec < 0 || iv_nsec < 0 || iv_nsec >= 1_000_000_000
            || vl_sec < 0 || vl_nsec < 0 || vl_nsec >= 1_000_000_000 {
            return -22; // EINVAL
        }
        let interval = (iv_sec as u64).saturating_mul(1_000_000_000).saturating_add(iv_nsec as u64);
        let value    = (vl_sec as u64).saturating_mul(1_000_000_000).saturating_add(vl_nsec as u64);
        (value, interval)
    };
    let pid = current_pid();
    if old_ptr != 0 {
        if !validate_user_buf(old_ptr, 32) { return -14; }
        // The vfs writes the itimerspec straight into user memory, so the
        // fetch has to happen while the old setting is still in place.
        let gmsg = make_vfs_msg(vfs::VFS_TIMERFD_GETTIME, &[fd as u64, old_ptr as u64]);
        let r = vfs_reply_val(&vfs::handle(&gmsg, pid));
        if r < 0 { return r; }
    }
    // 0 keeps its itimerspec meaning of "disarm". The vfs reads both flags.
    let msg = make_vfs_msg(vfs::VFS_TIMERFD_SETTIME, &[fd as u64, value_ns, interval_ns, flags as u64]);
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
///
/// `timespec` says which timeout struct `tv_ptr` points at: `select(2)` passes
/// a `struct timeval` (µs), `pselect6(2)` — the only form on AArch64 — a
/// `struct timespec` (ns). Reading a timespec as a timeval made every
/// pselect6 timeout 1000× too long.
///
/// `sigdata` (pselect6 only) points at `{ const sigset_t *ss; size_t ss_len }`;
/// a non-NULL `ss` is installed for the wait (see `masked_timed_wait`). Both
/// forms write the time not slept back into the timeout, as the Linux kernel
/// does (glibc/musl hide that from pselect callers by passing a copy).
fn sys_select(nfds: usize, rfds: usize, wfds: usize, efds: usize, tv_ptr: usize, timespec: bool, sigdata: usize) -> isize {
    if nfds > 1024 { return -22; } // EINVAL — matches relibc's FD_SETSIZE

    let ns = if tv_ptr == 0 {
        None
    } else if timespec {
        match read_user_timespec(tv_ptr) {
            Ok(ns) => Some(ns),
            Err(e) => return e,
        }
    } else {
        if !validate_user_buf(tv_ptr, 16) { return -14; }
        let (Some(tv_sec), Some(tv_usec)) = (ua::read_user::<i64>(tv_ptr), ua::read_user::<i64>(tv_ptr + 8))
            else { return -14; };
        if tv_sec < 0 || tv_usec < 0 { return -22; } // EINVAL
        Some((tv_sec as u64).saturating_mul(1_000_000_000).saturating_add((tv_usec as u64).saturating_mul(1_000)))
    };
    let (deadline, wb) = match ns {
        None => (u64::MAX, 0),
        Some(0) => (deadline_after_ns(0), 0), // zero timeout: never written back
        Some(ns) => (deadline_after_ns(ns), tv_ptr as u64 | if timespec { 0 } else { WB_TIMEVAL }),
    };
    let mut mask = NO_SIGMASK;
    if sigdata != 0 {
        if !validate_user_buf(sigdata, 16) { return -14; }
        let (Some(ss), Some(ss_len)) = (ua::read_user::<usize>(sigdata), ua::read_user::<usize>(sigdata + 8))
            else { return -14; };
        if ss != 0 {
            mask = match read_user_sigmask(ss, ss_len) { Ok(m) => m, Err(e) => return e };
        }
    }
    select_masked(nfds, rfds, wfds, efds, deadline, mask, wb)
}

/// select/pselect6 under `mask` with timeout write-back `wb`; also the
/// `restart_syscall` resume of one.
fn select_masked(nfds: usize, rfds: usize, wfds: usize, efds: usize, deadline: u64, mask: u64, wb: u64) -> isize {
    masked_timed_wait(mask, wb, deadline, || select_wait(nfds, rfds, wfds, efds, deadline, mask, wb))
}

/// The select/pselect6 loop proper, on an absolute `deadline` (`u64::MAX` =
/// none); also what `restart_syscall` resumes. Interrupted: EINTR when a
/// handler runs, else a restart on the SAME deadline (Linux gets there with
/// -ERESTARTNOHAND after writing the remainder back into the timeout).
/// As in Linux's `do_select`, with nothing ready a deliverable signal wins
/// over an expired deadline. `mask`/`wb` only travel into the restart block.
fn select_wait(nfds: usize, rfds: usize, wfds: usize, efds: usize, deadline: u64, mask: u64, wb: u64) -> isize {
    const POLLIN:  u32 = 0x0001;
    const POLLOUT: u32 = 0x0004;

    if nfds > 1024 { return -22; }
    let bytes = (nfds + 7) / 8;
    let pid = current_pid();
    let infinite = deadline == u64::MAX;

    let has_r = rfds != 0 && validate_user_buf(rfds, bytes);
    let has_w = wfds != 0 && validate_user_buf(wfds, bytes);
    let has_e = efds != 0 && validate_user_buf(efds, bytes);
    // An unreadable set is EFAULT up front (Linux `get_fd_set`); the loop
    // below re-reads the sets through the fault-tolerant copy.
    let mut probe = [0u8; 128];
    for (has, set) in [(has_r, rfds), (has_w, wfds), (has_e, efds)] {
        if has && unsafe { ua::copy_from_user(probe.as_mut_ptr(), set, bytes) } != 0 { return -14; }
    }

    loop {
        let gen0 = poll_gen();
        // fd_set is capped at 1024 bits (FD_SETSIZE) above, so 128 bytes
        // always covers `bytes`.
        let mut out_r = [0u8; 128];
        let mut out_w = [0u8; 128];
        let mut nready = 0isize;

        for fd in 0..nfds {
            let want_r = has_r && user_fdset_bit(rfds, fd);
            let want_w = has_w && user_fdset_bit(wfds, fd);
            if !want_r && !want_w { continue; }
            let requested = (if want_r { POLLIN } else { 0 }) | (if want_w { POLLOUT } else { 0 });
            let ev = probe_fd_events(pid, fd, requested);
            if want_r && ev & POLLIN  != 0 { out_r[fd / 8] |= 1 << (fd % 8); nready += 1; }
            if want_w && ev & POLLOUT != 0 { out_w[fd / 8] |= 1 << (fd % 8); nready += 1; }
        }

        if nready == 0 && interrupted() {
            sched::set_restart_block([RB_SELECT, nfds as u64, rfds as u64, wfds as u64, efds as u64, deadline, mask, wb]);
            return ERESTART_RESTARTBLOCK;
        }
        if nready > 0 || (!infinite && monotonic_ns() >= deadline) {
            let zero = [0u8; 128];
            if (has_r && unsafe { ua::copy_to_user(rfds, out_r.as_ptr(), bytes) } != 0)
                || (has_w && unsafe { ua::copy_to_user(wfds, out_w.as_ptr(), bytes) } != 0)
                || (has_e && unsafe { ua::copy_to_user(efds, zero.as_ptr(), bytes) } != 0) {
                return -14;
            }
            return nready;
        }

        poll_block(infinite, deadline, select_fds_mask(pid, nfds, rfds, wfds, has_r, has_w), gen0, || select_any_ready(pid, nfds, rfds, wfds, has_r, has_w));
    }
}

// ── rename / truncate / sendfile / itimer / sigpending / alarm ───────────────

/// sys_renameat(olddirfd, old_path_ptr, newdirfd, new_path_ptr, flags).
///
/// Both dirfds are honored (the atomic-write idiom — tempfile in an opened
/// directory, then `renameat(dirfd, name, ...)` with *relative* names — is
/// what cosmic-config/atomicwrites does for every state write; ignoring the
/// dirfds resolved those names against the cwd and answered ENOENT).
///
/// RENAMEAT2 flag support: RENAME_NOREPLACE passes through to the filesystem;
/// RENAME_EXCHANGE / RENAME_WHITEOUT answer EINVAL, which is exactly what
/// Linux reports for a filesystem without support — callers (rustix,
/// atomicwrites) treat that as "fall back to plain rename".
fn sys_renameat(olddirfd: usize, old_path_ptr: usize,
                newdirfd: usize, new_path_ptr: usize, flags: usize) -> isize {
    const RENAME_NOREPLACE: usize = 1;
    if flags & !RENAME_NOREPLACE != 0 { return -22; } // EINVAL
    let old = match resolve_at_path(olddirfd, old_path_ptr) { Ok(p) => p, Err(e) => return e };
    let new = match resolve_at_path(newdirfd, new_path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_RENAME,
                           &[old.ptr() as u64, new.ptr() as u64, flags as u64]);
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

    // Linux rejects a pipe on either end of sendfile(2)/copy_file_range(2)
    // with EINVAL, and callers rely on that to fall back to a plain read/write
    // loop. Emulating it here instead was actively harmful: a pipe read with
    // no data yet returns EAGAIN (-11), which the `n <= 0` break below turned
    // into a *successful* short copy — silent truncation — and a failing write
    // likewise reported success having moved nothing. std's io::copy reads that
    // 0 as EOF, which is how `printf '1\n2\n3\n' | tac` copied zero bytes into
    // its temp file and never noticed.
    for f in [in_fd, out_fd] {
        if let Some(vfs::VnodeKind::Pipe { .. }) = vfs::vfs_get_node_kind(pid, f) {
            return -22; // EINVAL
        }
    }

    // Sockets are not a source or sink this loop can drive (a socket write has
    // to go through net_server's NET_SEND and may block on a full ring); Linux
    // likewise refuses a socket `in_fd`. Answer EINVAL so the caller uses its
    // read/write fallback rather than getting EBADF out of the VFS, which does
    // not own these descriptors at all.
    if in_fd >= net_server::SOCK_FD_BASE || out_fd >= net_server::SOCK_FD_BASE {
        return -22; // EINVAL
    }

    // ── Console endpoints ─────────────────────────────────────────────────────
    // VFS's alloc_fd deliberately never hands out fds 0-2, so *any* VFS_READ /
    // VFS_WRITE naming one answers EBADF. sys_read/sys_write dodge that with a
    // console fast path; this loop talks to the VFS directly and so did not.
    //
    // That was invisible while a failed write merely broke the loop and
    // reported a short copy, but sendfile now propagates errors (so that a
    // failed copy is not mistaken for EOF — see the pipe note above), and the
    // EBADF became a hard error in userspace. It surfaced as
    // `tail -n3 <file> -> "tail: Bad file descriptor"`: std implements
    // io::copy(File, StdoutLock) with sendfile(2), and uutils tail only reaches
    // io::copy on its seek-backwards path, which it takes only when the file is
    // larger than st_blksize — the 4096 we report from stat(2). Hence the
    // sharp "4096 bytes passes, 4097 fails" boundary.
    //
    // `in_fd` on the console is a tty read, which sendfile(2) rejects (the
    // source must be mmap-able); EINVAL sends the caller to read/write.
    // `out_fd` on the console is the common case above and must actually work,
    // so it takes the same route sys_write does. sys_write cannot simply be
    // called here: its validate_user_buf rejects the kernel-side bounce buffer.
    if (in_fd == 0 && !vfs::fd_redirected(pid, 0)) || vfs::fd_is_console_stdio(pid, in_fd) {
        return -22; // EINVAL
    }
    let out_console = (matches!(out_fd, 1 | 2) && !vfs::fd_redirected(pid, out_fd))
        || vfs::fd_is_console_stdio(pid, out_fd);

    // If offset_ptr is given, seek in_fd to the caller-supplied offset.
    if offset_ptr != 0 {
        if !validate_user_buf(offset_ptr, 8) { return -14; }
        let off = match sched::uaccess::read_user::<u64>(offset_ptr) { Some(v) => v as usize, None => return -14 };
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
        // A read error must not masquerade as a clean end-of-input. Only a
        // genuine 0 (EOF) ends the transfer; a negative errno is reported when
        // nothing has been moved yet, matching sendfile(2).
        if n < 0 { if transferred == 0 { return n as isize; } else { break; } }
        if n == 0 { break; }
        let w = if out_console {
            console_write_user(&buf[..n as usize]);
            n
        } else {
            let wmsg = make_vfs_msg(vfs::VFS_WRITE,
                &[out_fd as u64, buf_ptr as u64, n as u64]);
            vfs_reply_val(&vfs::handle(&wmsg, pid))
        };
        if w <= 0 { if transferred == 0 && w < 0 { return w as isize; } else { break; } }
        transferred += w as usize;
    }

    // Update *offset_ptr to reflect how many bytes were consumed.
    if offset_ptr != 0 && transferred > 0 {
        let off = match sched::uaccess::read_user::<u64>(offset_ptr) { Some(v) => v, None => return -14 };
        unsafe { let _ = sched::uaccess::write_user::<u64>(offset_ptr, off + transferred as u64); }
    }

    transferred as isize
}

// ── splice / tee / vmsplice ──────────────────────────────────────────────────

const SPLICE_F_NONBLOCK: usize = 2;
/// SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT.
const SPLICE_F_ALL: usize = 0xF;
/// One splice moves at most a ring's worth: the pipe holds no more, and it
/// bounds the kernel bounce buffer (heap, never the kernel stack).
const SPLICE_CHUNK: usize = 16384;
/// "The non-pipe end answered EAGAIN" inside a splice attempt, kept apart
/// from the pipe end's EAGAIN (which parks) until the attempt returns.
const SPLICE_OTHER_EAGAIN: isize = -1011;

/// Write kernel bytes to `fd` the way write(2) routes a user buffer: the
/// console, a socket (net server), or any VFS descriptor, blocking as that
/// descriptor's own O_NONBLOCK says. sys_write itself cannot take a kernel
/// buffer (its validate_user_buf rejects one).
fn splice_write_out(pid: u32, fd: usize, buf: &[u8]) -> isize {
    if (matches!(fd, 1 | 2) && !vfs::fd_redirected(pid, fd))
        || (fd < net_server::SOCK_FD_BASE && vfs::fd_is_console_stdio(pid, fd)) {
        let jc = tty_server::console_write_check();
        if jc != 0 { return jc; }
        console_write_user(buf);
        return buf.len() as isize;
    }
    let ptr = buf.as_ptr() as u64;
    if fd >= net_server::SOCK_FD_BASE && fd < EPOLL_FD_BASE {
        let msg = make_vfs_msg(net_server::NET_SEND, &[fd as u64, ptr, buf.len() as u64, 0, 0, 0]);
        return net_block_timeo(pid, fd, net_fd_nonblock(pid, fd), true,
            || net_reply_val(&net_server::handle(&msg, pid)));
    }
    if fd >= EPOLL_FD_BASE { return -22; }
    let msg = make_vfs_msg(vfs::VFS_WRITE, &[fd as u64, ptr, buf.len() as u64]);
    block_until_ready(vfs::fd_nonblock(pid, fd),
        || vfs_reply_val(&vfs::handle(&msg, pid)),
        || vfs_block_hint(pid, fd, true))
}

/// True when `fd` cannot take an explicit offset (splice's ESPIPE rule for
/// the non-pipe end): sockets, epoll fds and the console.
fn splice_unseekable(pid: u32, fd: usize) -> bool {
    fd >= net_server::SOCK_FD_BASE
        || (fd <= 2 && !vfs::fd_redirected(pid, fd))
        || vfs::fd_is_console_stdio(pid, fd)
}

/// Read a user `loff_t *` (NULL = use the descriptor's own position).
fn splice_read_off(ptr: usize) -> Result<Option<u64>, isize> {
    if ptr == 0 { return Ok(None); }
    if !validate_user_buf(ptr, 8) { return Err(-14); }
    match sched::uaccess::read_user::<i64>(ptr) {
        Some(v) if v < 0 => Err(-22),
        Some(v) => Ok(Some(v as u64)),
        None => Err(-14),
    }
}

/// Park until `ready` holds (re-probed after the park is published, so a
/// producer's wake between the failed attempt and the park is not lost), a
/// signal arrives, or a wake on `mask` re-probes. The splice analogue of
/// `block_until_ready`'s park, separate because a splice attempt must not
/// run (and block in) the other end's I/O inside a published park.
fn splice_park(mask: u64, ready: impl Fn() -> bool) -> isize {
    if interrupted() { return ERESTARTSYS; }
    sched::block_on_poll_prepare_masked(u64::MAX, mask);
    if ready() || interrupted() { sched::block_on_poll_cancel(); } else { sched::block_on_poll_commit(); }
    0
}

/// splice(fd_in, off_in, fd_out, off_out, len, flags).
///
/// One end must be a pipe (EINVAL otherwise). Moves up to `len` bytes and
/// returns as soon as it has moved any: a full pipe (file -> pipe) or an
/// empty one (pipe -> file) ends the call with a short count instead of
/// waiting. Only a call that could move nothing at all blocks — or answers
/// EAGAIN under SPLICE_F_NONBLOCK or an O_NONBLOCK pipe. That is the whole
/// fix for the deadlock the old ENOSYS stub documented: uutils `cat` splices
/// file -> pipe -> stdout with a len far larger than the ring, and a
/// "move everything" loop blocked writing into its own full pipe with
/// nothing draining it.
///
/// `off_in`/`off_out` are ESPIPE on the pipe end; on the other end they
/// select pread/pwrite semantics (the descriptor's position is untouched
/// and `*off` advances by the count moved).
fn sys_splice(fd_in: usize, off_in: usize, fd_out: usize, off_out: usize, len: usize, flags: usize) -> isize {
    if flags & !SPLICE_F_ALL != 0 { return -22; }
    let pid = current_pid();
    // Both ends arrive translated (`fd_handle_args`): a socket end is its
    // handle, which no pipe is, and is read/written through the handle like
    // any socket I/O.
    let pipe_of = |fd: usize| if !vfs::is_handle(fd) { vfs::pipe_end_of(pid, fd) } else { None };
    let (pin, pout) = (pipe_of(fd_in), pipe_of(fd_out));
    if pin.is_none() && pout.is_none() { return -22; } // neither end is a pipe
    // A pipe end used in the wrong direction is not open for that access.
    if matches!(pin, Some((_, true))) || matches!(pout, Some((_, false))) { return -9; }
    if (pin.is_some() && off_in != 0) || (pout.is_some() && off_out != 0) { return -29; } // ESPIPE
    let mut oin = match splice_read_off(off_in) { Ok(o) => o, Err(e) => return e };
    let mut oout = match splice_read_off(off_out) { Ok(o) => o, Err(e) => return e };
    if (oin.is_some() && splice_unseekable(pid, fd_in)) || (oout.is_some() && splice_unseekable(pid, fd_out)) {
        return -29;
    }
    if len == 0 { return 0; }
    let len = len.min(SPLICE_CHUNK);
    let pipe_nonblock = |fd: usize| flags & SPLICE_F_NONBLOCK != 0 || vfs::fd_nonblock(pid, fd);
    // Read end waits for data (PIPE), write end for space (PIPE_WR).
    let tag = |ring: usize| sched::poll_tag(sched::poll_class::PIPE, ring as u32);
    let wtag = |ring: usize| sched::poll_tag(sched::poll_class::PIPE_WR, ring as u32);

    let r = match (pin, pout) {
        (Some((rin, _)), Some((rout, _))) => {
            if rin == rout { return -22; }
            let nonblock = pipe_nonblock(fd_in) || pipe_nonblock(fd_out);
            loop {
                match vfs::pipe_to_pipe(rin, rout, len, false) {
                    Ok(n) => break n as isize,
                    Err(-11) if !nonblock => {
                        let e = splice_park(tag(rin) | wtag(rout),
                            || vfs::pipe_ready(rin, false) && vfs::pipe_ready(rout, true));
                        if e != 0 { break e; }
                    }
                    Err(e) => break e as isize,
                }
            }
        }
        (None, Some((rout, _))) => {
            // file/socket/console -> pipe: reserve ring space first and read
            // no more than was reserved, so every byte read has a place in
            // the ring even when another writer races into the same pipe
            // while the source read blocks.
            let nonblock = pipe_nonblock(fd_out);
            let mut kbuf: Vec<u8> = alloc::vec![0u8; len];
            loop {
                let want = match vfs::pipe_reserve(rout, len) { Ok(s) => s, Err(e) => break e as isize };
                if want == 0 {
                    if nonblock { break -11; }
                    let e = splice_park(wtag(rout), || vfs::pipe_ready(rout, true));
                    if e != 0 { break e; }
                    continue;
                }
                let kp = kbuf.as_mut_ptr() as usize;
                let n = match oin {
                    Some(off) => pread_at(pid, fd_in, kp, want, off as usize),
                    None => sys_read_impl(fd_in, kp, want, true),
                };
                let got = if n > 0 { n as usize } else { 0 };
                let w = match vfs::pipe_commit(rout, kbuf.as_ptr(), got, want) {
                    Ok(w) => w,
                    Err(e) => break e as isize,
                };
                if n <= 0 { break if n == -11 { SPLICE_OTHER_EAGAIN } else { n }; }
                if let Some(o) = oin.as_mut() { *o += w as u64; }
                break w as isize;
            }
        }
        (Some((rin, _)), None) => {
            // pipe -> file/socket/console: peek, write, then consume what the
            // destination took, so a short or failed write loses nothing.
            let nonblock = pipe_nonblock(fd_in);
            let mut kbuf: Vec<u8> = alloc::vec![0u8; len];
            loop {
                let n = match vfs::pipe_peek(rin, kbuf.as_mut_ptr(), len) {
                    Ok(0) => break 0,
                    Ok(n) => n,
                    Err(-11) if !nonblock => {
                        let e = splice_park(tag(rin), || vfs::pipe_ready(rin, false));
                        if e != 0 { break e; }
                        continue;
                    }
                    Err(e) => break e as isize,
                };
                let w = match oout {
                    Some(off) => pwrite_at(pid, fd_out, kbuf.as_ptr() as usize, n, off as usize, 0),
                    None => splice_write_out(pid, fd_out, &kbuf[..n]),
                };
                if w < 0 { break if w == -11 { SPLICE_OTHER_EAGAIN } else { w }; }
                vfs::pipe_consume(rin, w as usize);
                if let Some(o) = oout.as_mut() { *o += w as u64; }
                break w;
            }
        }
        (None, None) => unreachable!(),
    };
    let r = if r == SPLICE_OTHER_EAGAIN { -11 } else { r };
    if r > 0 {
        if let Some(o) = oin { let _ = sched::uaccess::write_user::<u64>(off_in, o); }
        if let Some(o) = oout { let _ = sched::uaccess::write_user::<u64>(off_out, o); }
    }
    r
}

/// tee(fd_in, fd_out, len, flags): copy up to `len` bytes from one pipe to
/// another without consuming them. Same short-count and blocking rules as a
/// pipe -> pipe splice.
fn sys_tee(fd_in: usize, fd_out: usize, len: usize, flags: usize) -> isize {
    if flags & !SPLICE_F_ALL != 0 { return -22; }
    let pid = current_pid();
    let (rin, rout) = match (vfs::pipe_end_of(pid, fd_in), vfs::pipe_end_of(pid, fd_out)) {
        (Some((a, false)), Some((b, true))) if a != b => (a, b),
        _ => return -22,
    };
    if len == 0 { return 0; }
    let nonblock = flags & SPLICE_F_NONBLOCK != 0 || vfs::fd_nonblock(pid, fd_in) || vfs::fd_nonblock(pid, fd_out);
    let mask = sched::poll_tag(sched::poll_class::PIPE, rin as u32) | sched::poll_tag(sched::poll_class::PIPE_WR, rout as u32);
    loop {
        match vfs::pipe_to_pipe(rin, rout, len.min(SPLICE_CHUNK), true) {
            Ok(n) => return n as isize,
            Err(-11) if !nonblock => {
                let e = splice_park(mask, || vfs::pipe_ready(rin, false) && vfs::pipe_ready(rout, true));
                if e != 0 { return e; }
            }
            Err(e) => return e as isize,
        }
    }
}

/// vmsplice(fd, iov, nr_segs, flags): user memory into a pipe's write end,
/// or out of its read end. No page gifting here — it is a writev/readv on
/// the pipe, with SPLICE_F_NONBLOCK answered up front.
fn sys_vmsplice(fd: usize, iov: usize, nr_segs: usize, flags: usize) -> isize {
    if flags & !SPLICE_F_ALL != 0 { return -22; }
    let pid = current_pid();
    let (ring, is_write) = match vfs::pipe_end_of(pid, fd) { Some(p) => p, None => return -9 };
    if nr_segs == 0 { return 0; }
    if flags & SPLICE_F_NONBLOCK != 0 && !vfs::pipe_ready(ring, is_write) { return -11; }
    if is_write { sys_writev(fd, iov, nr_segs) } else { sys_readv(fd, iov, nr_segs) }
}

// ── pidfd ─────────────────────────────────────────────────────────────────────

/// PIDFD_NONBLOCK (== O_NONBLOCK).
const PIDFD_NONBLOCK: usize = 0o4000;

/// pidfd_open(pid, flags): a descriptor naming process `pid`, readable once
/// that process has exited.
///
/// tokio (brush's runtime) opens one for every child it spawns and, when this
/// succeeds, reaps the child from an epoll-registered pidfd instead of from
/// SIGCHLD: it waits for EPOLLIN and then calls waitpid(pid, WNOHANG). So the
/// readiness must never come early (the waitpid would find nothing and the
/// child would never be reaped) and never late (a pipeline would hang).
fn sys_pidfd_open(pid: usize, flags: usize) -> isize {
    if flags & !PIDFD_NONBLOCK != 0 { return -22; }
    let pid_i = pid as u32 as i32;
    if pid_i <= 0 { return -22; }
    let pid_k = match sched::pidns::arg(pid_i as u32) { Ok(k) => k, Err(e) => return e };
    if let Err(e) = sched::pidfd_target_check(pid_k) { return e; }
    vfs::install_pidfd(current_pid(), pid_k, flags as u32)
}

/// pidfd_send_signal(pidfd, sig, info, flags): kill(2) / rt_sigqueueinfo(2)
/// aimed through a pidfd, so the target cannot have been replaced by an
/// unrelated process. As on Linux, a zombie still accepts (and discards) a
/// signal; ESRCH once it has been reaped.
fn sys_pidfd_send_signal(pidfd: usize, sig: usize, info: usize, flags: usize) -> isize {
    if flags != 0 { return -22; }
    let pid = match vfs::pidfd_target(current_pid(), pidfd) { Ok(p) => p, Err(e) => return e as isize };
    if sig > 64 { return -22; }
    match sched::process_exit_state(pid) {
        (_, true) => return -3,    // ESRCH: reaped
        (true, false) => return 0, // zombie: nothing left to signal
        _ => {}
    }
    // sys_kill / sys_rt_sigqueueinfo take a pid number in the caller's pid
    // namespace; the pidfd holds a kernel pid. Not visible from here: ESRCH.
    let pid = sched::pidns::out(pid);
    if pid == 0 { return -3; }
    if info == 0 { return sys_kill(pid as usize, sig); }
    match sched::uaccess::read_user::<i32>(info) {
        Some(signo) if signo as usize != sig => -22,
        Some(_) => sys_rt_sigqueueinfo(None, pid as usize, sig, info),
        None => -14,
    }
}

/// pidfd_getfd(pidfd, targetfd, flags): a duplicate, in the caller's table, of
/// descriptor `targetfd` of the pidfd's process (close-on-exec, as on Linux).
/// Permission is Linux's PTRACE_MODE_ATTACH_REALCREDS check
/// (`sched::cred::ptrace_attach_check`).
fn sys_pidfd_getfd(pidfd: usize, targetfd: usize, flags: usize) -> isize {
    if flags != 0 { return -22; }
    let pid = match vfs::pidfd_target(current_pid(), pidfd) { Ok(p) => p, Err(e) => return e as isize };
    if sched::process_exit_state(pid).1 { return -3; }
    let r = sched::cred::ptrace_attach_check(pid);
    if r != 0 { return r; }
    // `targetfd` is a descriptor of the target's table. An epoll fd there is
    // a handle entry naming an instance: give the caller its own handle on
    // that instance, then a descriptor for it. Sockets and every VFS kind go
    // as an SCM_RIGHTS transfer (which resolves a socket descriptor itself).
    let h = vfs::resolve_user_fd(pid, targetfd);
    if is_epoll_handle(h) {
        let me = current_pid();
        let d = epoll_dup_into(h, pid, sched::current_tgid(), true);
        if d < 0 { return d; }
        return publish_handle(me, d as usize, 0);
    }
    net_server::dup_fd_across(pid, targetfd, sched::current_tgid())
}

/// sys_setitimer(which, new_ptr, old_ptr) — set an interval timer:
/// ITIMER_REAL (real time, SIGALRM), ITIMER_VIRTUAL (the process's user CPU
/// time, SIGVTALRM) or ITIMER_PROF (its user + system CPU time, SIGPROF),
/// each a reserved per-process timer (see `tty_server::set_itimer`). The two
/// CPU-time ones used to be accepted and ignored. Another `which` is EINVAL.
/// Parse a 32-byte `struct itimerval` (`{ it_interval, it_value }`, each a
/// `{ tv_sec: i64, tv_usec: i64 }` pair) into `(interval_ns, value_ns)`, or
/// `None` for a negative field / `tv_usec` outside `0..1_000_000` (EINVAL).
/// Nanoseconds, not ticks: a sub-10 ms it_value used to floor to 0 ticks,
/// which is "disarm".
fn parse_itimerval(buf: &[u8; 32]) -> Option<(u64, u64)> {
    let f = |o: usize| i64::from_ne_bytes(buf[o..o + 8].try_into().unwrap());
    let (iv_sec, iv_usec, va_sec, va_usec) = (f(0), f(8), f(16), f(24));
    if iv_sec < 0 || va_sec < 0 || !(0..1_000_000).contains(&iv_usec) || !(0..1_000_000).contains(&va_usec) {
        return None;
    }
    let ns = |s: i64, us: i64| (s as u64).saturating_mul(1_000_000_000).saturating_add(us as u64 * 1000);
    Some((ns(iv_sec, iv_usec), ns(va_sec, va_usec)))
}

/// Encode `(interval_ns, value_ns)` as a 32-byte `struct itimerval`. A
/// remaining value is rounded UP to the microsecond so an armed timer never
/// reads as 0 (disarmed).
fn itimerval_bytes(interval_ns: u64, value_ns: u64) -> [u8; 32] {
    let mut buf = [0u8; 32];
    let iv_us = (interval_ns + 999) / 1000;
    let va_us = (value_ns + 999) / 1000;
    buf[0..8].copy_from_slice(&((iv_us / 1_000_000) as i64).to_ne_bytes());
    buf[8..16].copy_from_slice(&((iv_us % 1_000_000) as i64).to_ne_bytes());
    buf[16..24].copy_from_slice(&((va_us / 1_000_000) as i64).to_ne_bytes());
    buf[24..32].copy_from_slice(&((va_us % 1_000_000) as i64).to_ne_bytes());
    buf
}

fn sys_setitimer(which: usize, new_ptr: usize, old_ptr: usize) -> isize {
    // struct itimerval: { it_interval: timeval(16), it_value: timeval(16) } = 32 bytes
    if new_ptr != 0 && !validate_user_buf(new_ptr, 32) { return -14; }
    if old_ptr != 0 && !validate_user_buf(old_ptr, 32) { return -14; }

    if which > 2 { return -22; } // EINVAL

    let pid = current_pid();

    let (interval_ns, value_ns) = if new_ptr != 0 {
        let mut buf = [0u8; 32];
        if !with_current_address_space(|as_| as_.read_user_buf(new_ptr, &mut buf)).unwrap_or(false) {
            return -14;
        }
        match parse_itimerval(&buf) { Some(v) => v, None => return -22 }
    } else {
        (0, 0)
    };

    // Arm the reserved slot directly (ns — no synthetic user-space pointer
    // round-trip; see tty_server::arm_timer's doc comment).
    let (old_interval_ns, old_value_ns) = match tty_server::set_itimer(pid, which as u32, interval_ns, value_ns) {
        Some(v) => v,
        None => return -22,
    };

    if old_ptr != 0 {
        let obuf = itimerval_bytes(old_interval_ns, old_value_ns);
        if !copy_to_user_bytes(old_ptr, &obuf) {
            return -14;
        }
    }
    0
}

/// sys_getitimer(which, cur_ptr) — get current interval timer state.
fn sys_getitimer(which: usize, cur_ptr: usize) -> isize {
    if which > 2 { return -22; } // EINVAL
    if !validate_user_buf(cur_ptr, 32) { return -14; }
    let pid = current_pid();
    let (interval_ns, value_ns) = match tty_server::get_itimer(pid, which as u32) {
        Some(v) => v,
        None => return -22,
    };
    let buf = itimerval_bytes(interval_ns, value_ns);
    if copy_to_user_bytes(cur_ptr, &buf) { 0 } else { -14 }
}

/// sys_sigpending(set_ptr) — return the set of pending signals.
fn sys_sigpending(set_ptr: usize) -> isize {
    if !validate_user_buf(set_ptr, 8) { return -14; }
    // Thread-pending plus process-level pending: a process-directed signal
    // that every thread currently masks is parked on the leader
    // (shared_signal_pending) but is still "pending" to sigpending(2).
    let pending = pending_signals() | sched::shared_pending_signals();
    if copy_to_user_bytes(set_ptr, &pending.to_ne_bytes()) {
        0
    } else {
        -14
    }
}

/// sys_alarm(seconds) — schedule SIGALRM after `seconds` seconds (x86-64 only).
#[cfg(not(target_arch = "aarch64"))]
fn sys_alarm(seconds: usize) -> isize {
    let pid = current_pid();
    const NS: u64 = 1_000_000_000;
    let value_ns = (seconds as u64).saturating_mul(NS);

    // One-shot (no interval) — arm the reserved ITIMER_REAL slot directly.
    let (_, old_value_ns) = tty_server::set_real_itimer(pid, 0, value_ns);

    // Real alarm() returns the number of seconds remaining on any previous
    // alarm, rounded up so a caller never sees "0 seconds left" for an
    // alarm that's about to fire (matches glibc/Linux behavior).
    ((old_value_ns + NS - 1) / NS) as isize
}

// ── fork / clone ──────────────────────────────────────────────────────────────

/// One clone request, whichever syscall (clone, clone3, vfork) made it.
struct CloneReq {
    /// Pre-created user namespace id for CLONE_NEWUSER (0 = none); set by do_clone.
    userns: sched::ns::NsId,
    /// Pre-created pid namespace id for CLONE_NEWPID (0 = none); set by do_clone.
    pidns: sched::ns::NsId,
    netns: sched::ns::NsId,
    ipcns: sched::ns::NsId,
    /// CLONE_* flags; the low byte (CSIGNAL) is the signal a new process
    /// sends its parent when it exits (SIGCHLD normally, 0 for none).
    flags: usize,
    /// Child's user stack pointer (0 = share/copy the parent's).
    stack: usize,
    /// CLONE_PARENT_SETTID word.
    ptid:  usize,
    /// CLONE_CHILD_SETTID / CLONE_CHILD_CLEARTID word.
    ctid:  usize,
    /// CLONE_SETTLS value.
    tls:   usize,
    /// CLONE_PIDFD: where the new pidfd is stored (an `int`).
    pidfd: usize,
    /// clone3 `set_tid`: the pid the child must get.
    set_tid: Option<u32>,
}

const CLONE_VM:             usize = 0x0000_0100;
const CLONE_FS:             usize = 0x0000_0200;
const CLONE_FILES:          usize = 0x0000_0400;
const CLONE_SIGHAND:        usize = 0x0000_0800;
const CLONE_PIDFD:          usize = 0x0000_1000;
const CLONE_PTRACE:         usize = 0x0000_2000;
const CLONE_VFORK:          usize = 0x0000_4000;
const CLONE_PARENT:         usize = 0x0000_8000;
const CLONE_THREAD:         usize = 0x0001_0000;
const CLONE_NEWNS:          usize = 0x0002_0000;
const CLONE_SYSVSEM:        usize = 0x0004_0000;
const CLONE_SETTLS:         usize = 0x0008_0000;
const CLONE_PARENT_SETTID:  usize = 0x0010_0000;
const CLONE_CHILD_CLEARTID: usize = 0x0020_0000;
const CLONE_DETACHED:       usize = 0x0040_0000;
const CLONE_UNTRACED:       usize = 0x0080_0000;
const CLONE_CHILD_SETTID:   usize = 0x0100_0000;
const CLONE_NEWCGROUP:      usize = 0x0200_0000;
const CLONE_NEWUTS:         usize = 0x0400_0000;
const CLONE_NEWIPC:         usize = 0x0800_0000;
const CLONE_NEWUSER:        usize = 0x1000_0000;
const CLONE_NEWPID:         usize = 0x2000_0000;
const CLONE_NEWNET:         usize = 0x4000_0000;
const CLONE_IO:             usize = 0x8000_0000;
/// clone3-only flags (above bit 31).
const CLONE_CLEAR_SIGHAND:  u64 = 0x1_0000_0000;
const CLONE_INTO_CGROUP:    u64 = 0x2_0000_0000;
/// The low byte of clone(2)'s flags: the exit signal.
const CSIGNAL:              usize = 0xff;

/// sys_clone_or_fork — dispatches `fork()` and `clone()`.
///
/// Register convention (matching Linux):
///   x86_64:  a0 = flags, a1 = child_stack, a2 = ptid, a3 = ctid, a4 = tls
///   AArch64: a0 = flags, a1 = child_stack, a2 = ptid, a3 = tls,  a4 = ctid
///
/// On AArch64 there is no separate `fork` syscall; musl uses `clone(SIGCHLD)`
/// which has CLONE_VM clear.  On x86-64 `FORK` (57) routes directly in the
/// dispatch table; this function sees `CLONE` (56) and `VFORK` (58).
fn sys_clone_or_fork(
    a0:          usize,
    a1:          usize,
    a2:          usize,
    a3:          usize,
    a4:          usize,
    frame_ptr:   usize,
) -> isize {
    #[cfg(target_arch = "x86_64")]
    let (flags, stack, ptid, ctid, tls) = (a0, a1, a2, a3, a4);
    #[cfg(target_arch = "aarch64")]
    let (flags, stack, ptid, tls, ctid) = (a0, a1, a2, a3, a4);

    // clone(2) takes 32 flag bits; the rest of the register is not looked at.
    let flags = flags & 0xFFFF_FFFF;
    // CLONE_PIDFD returns the child's pidfd through `ptid` (a2 on both
    // arches), so it cannot be combined with CLONE_PARENT_SETTID (same slot).
    if flags & CLONE_PIDFD != 0 && flags & CLONE_PARENT_SETTID != 0 {
        return -22;
    }
    let pidfd = if flags & CLONE_PIDFD != 0 { ptid } else { 0 };
    do_clone(CloneReq { flags, stack, ptid, ctid, tls, pidfd, set_tid: None, userns: 0, pidns: 0, netns: 0, ipcns: 0 }, frame_ptr)
}

/// vfork(2): `clone(CLONE_VM | CLONE_VFORK | SIGCHLD)` — the child runs on
/// the parent's memory and stack, and the parent sleeps until it execs or
/// exits. x86_64 only; aarch64 has no vfork syscall (libc uses clone).
#[cfg(not(target_arch = "aarch64"))]
fn sys_vfork(frame_ptr: usize) -> isize {
    const SIGCHLD: usize = 17;
    do_clone(CloneReq { flags: CLONE_VM | CLONE_VFORK | SIGCHLD, stack: 0, ptid: 0, ctid: 0,
                        tls: 0, pidfd: 0, set_tid: None, userns: 0, pidns: 0, netns: 0, ipcns: 0 }, frame_ptr)
}

/// clone3(uargs, size): `struct clone_args` translated onto the clone path.
///
/// Size versioning as Linux's `copy_struct_from_user`: at least
/// CLONE_ARGS_SIZE_VER0 (64) bytes, at most a page; fields this kernel does
/// not know (past VER2, 88 bytes) must be zero (E2BIG otherwise).
/// `set_tid` picks the child's pid (one pid namespace level here, root
/// only: EINVAL for more levels or an out-of-range pid, EPERM unprivileged,
/// EEXIST when taken). `CLONE_INTO_CGROUP` is not supported (EINVAL).
fn sys_clone3(uargs: usize, size: usize, frame_ptr: usize) -> isize {
    const VER0: usize = 64;
    const VER2: usize = 88;
    if size < VER0 { return -22; }
    if size > 4096 { return -7; } // E2BIG
    let mut raw = [0u8; VER2];
    let known = size.min(VER2);
    if !validate_user_buf(uargs, size) { return -14; }
    if unsafe { sched::uaccess::copy_from_user(raw.as_mut_ptr(), uargs, known) } != 0 { return -14; }
    if size > VER2 {
        // Trailing bytes from a newer userspace must all be zero.
        let mut rest = [0u8; 256];
        let mut off = VER2;
        while off < size {
            let n = (size - off).min(rest.len());
            if unsafe { sched::uaccess::copy_from_user(rest.as_mut_ptr(), uargs + off, n) } != 0 {
                return -14;
            }
            if rest[..n].iter().any(|&b| b != 0) { return -7; } // E2BIG
            off += n;
        }
    }
    let f = |i: usize| u64::from_ne_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
    let (flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls,
         set_tid, set_tid_size, cgroup) =
        (f(0), f(1), f(2), f(3), f(4), f(5), f(6), f(7), f(8), f(9), f(10));

    // copy_clone_args_from_user
    if set_tid_size > 32 { return -22; }
    if set_tid == 0 && set_tid_size > 0 { return -22; }
    if set_tid != 0 && set_tid_size == 0 { return -22; }
    if exit_signal & !(CSIGNAL as u64) != 0 || exit_signal > 64 { return -22; }
    if flags & CLONE_INTO_CGROUP != 0 && (cgroup > i32::MAX as u64 || size < VER2) { return -22; }
    // clone3_args_valid
    const LEGACY: u64 = (CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_PIDFD
        | CLONE_PTRACE | CLONE_VFORK | CLONE_PARENT | CLONE_THREAD | CLONE_NEWNS
        | CLONE_SYSVSEM | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID
        | CLONE_DETACHED | CLONE_UNTRACED | CLONE_CHILD_SETTID | CLONE_NEWCGROUP
        | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET
        | CLONE_IO) as u64;
    if flags & !(LEGACY | CLONE_CLEAR_SIGHAND | CLONE_INTO_CGROUP) != 0 { return -22; }
    // The exit signal has its own field: CSIGNAL bits in flags are invalid,
    // and so is CLONE_DETACHED (a long-gone legacy flag).
    if flags & (CSIGNAL | CLONE_DETACHED) as u64 != 0 { return -22; }
    if flags & CLONE_SIGHAND as u64 != 0 && flags & CLONE_CLEAR_SIGHAND != 0 { return -22; }
    if flags & (CLONE_THREAD | CLONE_PARENT) as u64 != 0 && exit_signal != 0 { return -22; }
    // clone3_stack_valid: both or neither; the stack grows down from the top.
    let sp = if stack == 0 {
        if stack_size != 0 { return -22; }
        0
    } else {
        if stack_size == 0 { return -22; }
        match stack.checked_add(stack_size) {
            Some(top) if validate_user_buf(stack as usize, stack_size as usize) => top as usize,
            _ => return -14,
        }
    };
    // Not supported here: placing the child in a cgroup.
    if flags & CLONE_INTO_CGROUP != 0 { return -22; }
    // set_tid: one entry per pid-namespace level, and there is one level.
    let set_tid_pid = if set_tid_size != 0 {
        if set_tid_size > 1 { return -22; }
        let mut v = [0u8; 4];
        if unsafe { sched::uaccess::copy_from_user(v.as_mut_ptr(), set_tid as usize, 4) } != 0 { return -14; }
        let want = i32::from_ne_bytes(v);
        if want < 1 || want as u32 >= sched::PID_MAX { return -22; }
        // Linux: CAP_CHECKPOINT_RESTORE (or CAP_SYS_ADMIN) in the owning
        // user namespace.
        if sched::current_euid() != 0 { return -1; }
        Some(want as u32)
    } else { None };
    // Default handlers in the child: what the child of a fork that is about
    // to exec wants. Dispositions are copied at fork; not modelled yet, so
    // refuse rather than silently keep handlers the caller asked to drop.
    if flags & CLONE_CLEAR_SIGHAND != 0 { return -22; }

    let flags = (flags as usize) | exit_signal as usize;
    let pidfd_ptr = if flags & CLONE_PIDFD != 0 { pidfd as usize } else { 0 };
    if let Some(p) = set_tid_pid {
        if let Err(e) = sched::reserve_pid(p) { return e; }
    }
    let r = do_clone(CloneReq { flags, stack: sp, ptid: parent_tid as usize, ctid: child_tid as usize,
                                tls: tls as usize, pidfd: pidfd_ptr, set_tid: set_tid_pid, userns: 0, pidns: 0, netns: 0, ipcns: 0 }, frame_ptr);
    if let Some(p) = set_tid_pid { sched::release_reserved_pid(p); }
    r
}

/// The common clone body. CLONE_PIDFD is handled here: the descriptor is
/// reserved in the parent's table *before* the child exists, as Linux does,
/// so a full table fails the whole clone with EMFILE and no child — rather
/// than leaving a child the caller has no pidfd for.
fn do_clone(req: CloneReq, frame_ptr: usize) -> isize {
    let flags = req.flags;
    // A pidfd names a process; thread pidfds (PIDFD_THREAD) are not offered.
    if flags & CLONE_PIDFD != 0 && flags & CLONE_THREAD != 0 { return -22; }
    // Linux's copy_process consistency rules.
    if flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0 { return -22; }
    if flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0 { return -22; }

    // Namespaces: only CLONE_NEWUSER is implemented (nsbox lane L1); every
    // other CLONE_NEW* is refused instead of silently ignored.
    if flags & (CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS) != 0 { return -22; }
    // Pid namespaces (lane nspid). Linux copy_process rules: NEWPID excludes
    // CLONE_THREAD/SIGHAND; here also CLONE_VM (the vfork-style arm has no
    // namespace plumbing). A thread or CLONE_PARENT child cannot be placed
    // when unshare(NEWPID) left pid_for_children differing from our own
    // namespace; CLONE_PARENT from a namespace init is refused too.
    if flags & CLONE_NEWUSER != 0 && sched::current_root_arc().is_some() { return -1; } // EPERM when chrooted
    let (cur_pidns, cur_pidns_child) = sched::pidns::cur_and_child();
    if flags & CLONE_NEWPID != 0 {
        if flags & (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) != 0 { return -22; }
        if flags & CLONE_NEWUSER == 0 && !sched::caps::ns_capable(21) { return -1; }
        if cur_pidns_child != cur_pidns { return -22; }
        if req.set_tid.is_some() { return -22; }
    }
    if flags & (CLONE_THREAD | CLONE_PARENT) != 0 && cur_pidns_child != cur_pidns { return -22; }
    if flags & CLONE_PARENT != 0 && cur_pidns != 0 && sched::pidns::self_v(sched::current_tgid()) == 1 { return -22; }
    if req.set_tid.is_some() && cur_pidns_child != 0 { return -22; }
    // NEWNET/NEWIPC: need CAP_SYS_ADMIN in the (possibly new) user namespace;
    // a thread or a shared-VM child cannot get its own.
    if flags & (CLONE_NEWNET | CLONE_NEWIPC) != 0 {
        if flags & (CLONE_THREAD | CLONE_VM) != 0 { return -22; }
        if flags & CLONE_NEWUSER == 0 && !sched::caps::ns_capable(21) { return -1; }
    }
    // Linux: CLONE_NEWUSER excludes CLONE_THREAD and CLONE_FS; the vfork-style
    // CLONE_VM arm has no namespace plumbing.
    if flags & CLONE_NEWUSER != 0 && flags & (CLONE_THREAD | CLONE_FS | CLONE_VM) != 0 { return -22; }
    // The child's user namespace, created up front so a full table or too deep
    // a nesting fails the whole clone. This reference is the creator's; the
    // child takes its own, and ours drops when the clone returns.
    let new_user = if flags & CLONE_NEWUSER != 0 {
        let (p, eu, eg) = (sched::ns::current_userns(), sched::current_euid(), sched::current_egid());
        match sched::ns::new_userns(p, eu, eg) { Ok(n) => Some(n), Err(e) => return e }
    } else { None };
    let mut req = req;
    req.userns = new_user.as_ref().map_or(0, |n| n.id());
    let new_pid_ns = if flags & CLONE_NEWPID != 0 {
        match sched::ns::new_pidns(cur_pidns) { Ok(n) => Some(n), Err(e) => return e }
    } else { None };
    req.pidns = new_pid_ns.as_ref().map_or(0, |n| n.id());
    let new_net = if flags & CLONE_NEWNET != 0 { match sched::ns::new_flat() { Ok(n) => Some(n), Err(e) => return e } } else { None };
    let new_ipc = if flags & CLONE_NEWIPC != 0 { match sched::ns::new_flat() { Ok(n) => Some(n), Err(e) => return e } } else { None };
    req.netns = new_net.as_ref().map_or(0, |n| n.id());
    req.ipcns = new_ipc.as_ref().map_or(0, |n| n.id());

    let pidfd = if flags & CLONE_PIDFD != 0 {
        if !validate_user_buf(req.pidfd, 4) { return -14; }
        prefault_user(req.pidfd, 4);
        // Placeholder: names no process until the child exists (pid 0 is
        // never a live process, so the window cannot signal anything).
        let fd = vfs::install_pidfd(current_pid(), 0, 0);
        if fd < 0 { return fd; } // EMFILE / ENFILE
        Some(fd as usize)
    } else {
        None
    };

    let ret = clone_or_fork_inner(&req, pidfd, frame_ptr);
    if let Some(fd) = pidfd {
        if ret > 0 {
            vfs::pidfd_retarget(current_pid(), fd, ret as u32);
            let v = (fd as i32).to_ne_bytes();
            let _ = with_current_address_space_mut(|as_| as_.write_user_buf(req.pidfd, &v));
        } else {
            let _ = sys_close(fd);
        }
    }
    ret
}

fn clone_or_fork_inner(req: &CloneReq, pidfd: Option<usize>, frame_ptr: usize) -> isize {
    let CloneReq { flags, stack: child_stack, ptid, ctid, tls, .. } = *req;

    // The tid words are stored through the VMA tables inside clone_thread
    // (no user access under RUN_QUEUE), which cannot fault a lazy page in.
    // Make them resident here, with no lock held. musl's `&new->tid` shares
    // a page with the `struct pthread` it has just initialised, so this is
    // normally a no-op, but a pointer into an untouched mapping would
    // otherwise be silently skipped.
    if flags & CLONE_PARENT_SETTID != 0 { prefault_user(ptid, 4); }
    if flags & CLONE_CHILD_SETTID  != 0 { prefault_user(ctid, 4); }

    // The child got a copy of the parent's fd table, placeholder pidfd
    // included; Linux installs the pidfd after copying the table, so the
    // child never holds it. Drop it from the child's copy.
    let drop_child_pidfd = |child_pid: u32| {
        if let Some(fd) = pidfd {
            let m = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]);
            let _ = vfs::handle(&m, child_pid);
        }
    };

    if flags & CLONE_VM != 0 {
        // Identify the parent by tgid: its fd table is keyed there, not by the
        // (possibly non-leader) forking thread's pid.
        let parent_pid = sched::tgid_of(current_pid());
        // A vfork-style child gets its own fd table (below): EAGAIN up front
        // when the vfs pool has none left, as for fork.
        if flags & CLONE_THREAD == 0 && !vfs::fd_table_slot_free() { return -11; }
        let mut child = 0u32;
        let mut threaded = 0u32;
        let ret = clone_thread(flags, req.set_tid, child_stack, tls, ptid, ctid, frame_ptr, |child_pid| {
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
                child = child_pid;
                fork_dup_tables(parent_pid, child_pid);
                drop_child_pidfd(child_pid);
            } else {
                // A thread joins its creator's private fd table, if any.
                if sched::fdkey::any() && sched::fdkey::inherit(current_pid(), child_pid) { threaded = child_pid; }
            }
        });
        if ret < 0 {
            undo_unborn_child(child);
            if threaded != 0 { let _ = sched::fdkey::drop(threaded); }
        }
        ret
    } else {
        // fd tables are keyed by tgid, so the parent must be identified by its
        // thread-group id — a fork issued by a non-leader thread (e.g. a tokio
        // worker calling std's pre_exec fork path) otherwise names a pid the
        // fd-table search never matches, and the child inherits an empty table.
        let parent_pid = sched::tgid_of(current_pid());
        if !vfs::fd_table_slot_free() { return -11; } // EAGAIN, as for FORK
        let opts = sched::ForkOpts {
            vfork: flags & CLONE_VFORK != 0,
            child_stack,
            tls: if flags & CLONE_SETTLS != 0 { Some(tls as u64) } else { None },
            exit_signal: (flags & CSIGNAL) as u8,
            set_tid: req.set_tid,
            share_fs: flags & CLONE_FS != 0,
            userns: req.userns,
            pidns: req.pidns,
            netns: req.netns,
            ipcns: req.ipcns,
        };
        // Duplicate the fd table before the child becomes runnable (see the
        // FORK arm of syscall_dispatch for the SMP race this prevents).
        let mut child = 0u32;
        let ret = sched::fork_current_with(frame_ptr, opts, |child_pid| {
            child = child_pid;
            fork_dup_tables(parent_pid, child_pid);
            drop_child_pidfd(child_pid);
        });
        if ret < 0 { undo_unborn_child(child); }
        // CLONE_PARENT_SETTID on a plain fork names a word in the PARENT's
        // memory (the child got its own copy at fork), and only the parent
        // returns here with a positive pid.
        if ret > 0 && flags & CLONE_PARENT_SETTID != 0 && ptid != 0 {
            let tid = (ret as u32).to_ne_bytes();
            let _ = with_current_address_space_mut(|as_| as_.write_user_buf(ptid, &tid));
        }
        ret
    }
}


