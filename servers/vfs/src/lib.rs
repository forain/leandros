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

#![no_std]

use ipc::{Message, port};
use spin::Mutex;

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

fn ok_reply()        -> Message { make_reply(0) }
fn err_reply(e: i32) -> Message { make_reply(e as i64) }
fn val_reply(v: u64) -> Message { make_reply(v as i64) }

// ── Writable tmpfs pool ───────────────────────────────────────────────────────

const MAX_TMP_FILES: usize = 32;
const MAX_TMP_SIZE:  usize = 4096;
const MAX_TMP_PATH:  usize = 64;

struct TmpFileEntry {
    path:     [u8; MAX_TMP_PATH],
    path_len: usize,
    data:     [u8; MAX_TMP_SIZE],
    len:      usize,
    in_use:   bool,
    is_dir:   bool,
}

impl TmpFileEntry {
    const fn empty() -> Self {
        Self { path: [0u8; MAX_TMP_PATH], path_len: 0,
               data: [0u8; MAX_TMP_SIZE], len: 0,
               in_use: false, is_dir: false }
    }
}

static TMP_FILES: Mutex<[TmpFileEntry; MAX_TMP_FILES]> =
    Mutex::new([const { TmpFileEntry::empty() }; MAX_TMP_FILES]);

// ── Vnode kinds ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum VnodeKind {
    None,
    /// Static read-only RamFS file.
    RamFile { data: &'static [u8], pos: usize },
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
}

// ── Pipe ring buffers ─────────────────────────────────────────────────────────

const PIPE_RING_SIZE: usize = 4096;
const MAX_PIPES:      usize = 16;

struct PipeRing {
    buf:         [u8; PIPE_RING_SIZE],
    read_pos:    usize,
    write_pos:   usize,
    count:       usize,
    read_open:   bool,
    write_open:  bool,
}

impl PipeRing {
    const fn new() -> Self {
        Self {
            buf: [0u8; PIPE_RING_SIZE],
            read_pos: 0, write_pos: 0, count: 0,
            read_open: false, write_open: false,
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

// ── eventfd counters ──────────────────────────────────────────────────────────

const MAX_EVENTFDS: usize = 16;
// u64::MAX = free slot sentinel.
static EVENTFD_COUNTERS: Mutex<[u64; MAX_EVENTFDS]> = Mutex::new([u64::MAX; MAX_EVENTFDS]);

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
        self.fds.iter().position(|f| !f.in_use)
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

pub fn set_initrd(base: usize, size: usize) {
    INITRD_BASE.store(base, atomic::Ordering::SeqCst);
    INITRD_SIZE.store(size, atomic::Ordering::SeqCst);
}

pub fn set_framebuffer(base: u64, width: u32, height: u32, pitch: u32) {
    FB_BASE.store(base, atomic::Ordering::SeqCst);
    FB_WIDTH.store(width, atomic::Ordering::SeqCst);
    FB_HEIGHT.store(height, atomic::Ordering::SeqCst);
    FB_PITCH.store(pitch, atomic::Ordering::SeqCst);
}

use core::sync::atomic;

// ── Static RamFS ──────────────────────────────────────────────────────────────

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
    // /etc
    RamEntry { path: b"/etc/motd",
               data: b"Welcome to Cyanos!\nType 'help' for available commands.\n" },
    RamEntry { path: b"/etc/passwd",
               data: b"root:x:0:0:root:/root:/bin/sh\ndaemon:x:1:1:daemon:/:/bin/false\n" },
    RamEntry { path: b"/etc/group",
               data: b"root:x:0:root\ndaemon:x:1:\n" },
    RamEntry { path: b"/etc/hostname", data: b"cyanos\n" },
    RamEntry { path: b"/etc/hosts",
               data: b"127.0.0.1\tlocalhost\n::1\t\tlocalhost\n127.0.0.1\tcyanos\n" },
    RamEntry { path: b"/etc/resolv.conf",
               data: b"nameserver 8.8.8.8\nnameserver 8.8.4.4\n" },
    RamEntry { path: b"/etc/services",
               data: b"http\t80/tcp\nhttps\t443/tcp\nssh\t22/tcp\nftp\t21/tcp\n" },
    RamEntry { path: b"/etc/protocols",
               data: b"ip\t0\tIP\ntcp\t6\tTCP\nudp\t17\tUDP\nicmp\t1\tICMP\n" },
    RamEntry { path: b"/etc/nsswitch.conf",
               data: b"hosts: files dns\npasswd: files\ngroup: files\n" },
    RamEntry { path: b"/etc/os-release",
               data: b"NAME=\"Cyanos\"\nVERSION=\"1.0\"\nID=cyanos\nPRETTY_NAME=\"Cyanos 1.0\"\n" },
    RamEntry { path: b"/proc/version",
               data: b"Linux version 6.0.0-cyanos (Cyanos Project) (gcc 13.0)\n" },
    RamEntry { path: b"/proc/cpuinfo",
               data: b"processor\t: 0\nmodel name\t: Cyanos Virtual CPU\ncpu MHz\t\t: 1000.000\n\
                       cache size\t: 4096 KB\nflags\t\t: fpu vme de pse tsc msr pae mce\n" },
    RamEntry { path: b"/proc/filesystems",
               data: b"nodev\ttmpfs\nnodev\tramfs\nnodev\tprocfs\n\text2\n" },
    RamEntry { path: b"/proc/mounts",
               data: b"proc /proc procfs rw 0 0\ntmpfs /tmp tmpfs rw 0 0\n" },
    RamEntry { path: b"/proc/net/dev",
               data: b"Inter-|   Receive                                       |  Transmit\n\
                       face |bytes packets errs drop fifo frame compressed multicast\
                       |bytes packets errs drop fifo colls carrier compressed\n\
                   lo:      0       0    0    0    0     0          0         0       0       0    0    0    0     0       0          0\n" },
    RamEntry { path: b"/proc/net/if_inet6",  data: b"" },
    RamEntry { path: b"/proc/net/fib_trie",  data: b"Main:\n  +-- 0.0.0.0/0\n" },
    RamEntry { path: b"/proc/sys/kernel/hostname",   data: b"cyanos\n" },
    RamEntry { path: b"/proc/sys/kernel/ostype",     data: b"Linux\n" },
    RamEntry { path: b"/proc/sys/kernel/osrelease",  data: b"6.0.0-cyanos\n" },
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
    let tmp = TMP_FILES.lock();
    tmp.iter().any(|e| e.in_use && e.is_dir && e.path_len == plen && &e.path[..plen] == &pbuf[..plen])
}

// ── Message dispatch ──────────────────────────────────────────────────────────

pub fn handle(msg: &Message, caller_pid: u32) -> Message {
    match msg.tag {
        VFS_OPEN         => handle_open(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        VFS_READ         => handle_read(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_WRITE        => handle_write(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_CLOSE        => handle_close(caller_pid, arg(msg,0) as usize),
        VFS_LSEEK        => handle_lseek(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as i64, arg(msg,2) as u32),
        VFS_PIPE         => handle_pipe(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_DUP2         => handle_dup2(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_FCNTL        => handle_fcntl(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_FORK_DUP     => handle_fork_dup(arg(msg,0) as u32, arg(msg,1) as u32),
        VFS_EXEC_CLOEXEC => handle_exec_cloexec(arg(msg,0) as u32),
        VFS_CLOSE_ALL    => handle_close_all(arg(msg,0) as u32),
        VFS_GETDENTS64   => handle_getdents64(caller_pid, arg(msg,0) as usize,
                                               arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_ALLOC_FD     => handle_alloc_fd(caller_pid, arg(msg,0) as usize),
        VFS_UNLINK       => handle_unlink(arg(msg,0) as usize),
        VFS_MKDIR        => handle_mkdir(arg(msg,0) as usize),
        VFS_FTRUNCATE    => handle_ftruncate(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
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
        _                    => err_reply(-38),
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

// O_CREAT, O_TRUNC, O_WRONLY, O_RDWR flags
const O_WRONLY:    u32 = 0x01;
const O_RDWR:      u32 = 0x02;
const O_CREAT:     u32 = 0x40;
const O_TRUNC:     u32 = 0x200;
const O_APPEND:    u32 = 0x400;
#[allow(dead_code)]
const O_DIRECTORY: u32 = 0x10000;

/// Return true if `path` starts with the prefix `/tmp/` or equals `/tmp`.
fn is_tmp_path(path: &[u8]) -> bool {
    path == b"/tmp" || path.starts_with(b"/tmp/")
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

/// Generate dynamic /proc/ system-wide entries (meminfo, uptime, loadavg, stat).
fn gen_proc_system(path: &[u8]) -> Option<VnodeKind> {
    let mut buf = [0u8; TMP_BUF_SIZE];
    let len = gen_proc_system_content(path, &mut buf)?;
    let mut tmp = TMP_FILES.lock();
    let idx = tmp.iter().position(|e| !e.in_use)?;
    tmp[idx] = TmpFileEntry::empty();
    tmp[idx].in_use = true;
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
    tmp[idx].in_use   = true;
    tmp[idx].is_dir   = false;
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
        p = write_lit(buf, p, b"Name:\tcyanos\nState:\tR (running)\nPid:\t");
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
        p = write_lit(buf, p, b" (cyanos) R ");
        p = write_u32(buf, p, ppid);
        p = write_lit(buf, p, b" ");
        p = write_u32(buf, p, pgid);
        p = write_lit(buf, p, b" 0 0 0 0 0 0 0 0 0 0 0 0 0 0 20 0 1 0 ");
        p = write_u32(buf, p, uptime_sec as u32);
        p = write_lit(buf, p, b" 8388608 2048 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 0\n");
        return Some(p);
    }

    if path == b"/proc/self/cmdline" || path.ends_with(b"/cmdline") {
        let s = b"cyanos\x00";
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

    None
}

fn handle_open(pid: u32, path_ptr: usize, flags: u32) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) {
        Some(r) => r,
        None    => return err_reply(-14),
    };
    let mut path = &pbuf[..plen];

    // Basic normalization: . to /, strip trailing slash
    if path == b"." {
        path = b"/";
    } else if path.len() > 1 && path.ends_with(b"/") {
        path = &path[..path.len()-1];
    }

    let kind = if path == b"/dev/null" {
        VnodeKind::DevNull
    } else if path == b"/dev/zero" {
        VnodeKind::DevZero
    } else if path == b"/dev/urandom" || path == b"/dev/random" {
        VnodeKind::DevUrandom
    } else if path == b"/dev/stdin" {
        VnodeKind::DevStdio { target_fd: 0 }
    } else if path == b"/dev/stdout" {
        VnodeKind::DevStdio { target_fd: 1 }
    } else if path == b"/dev/stderr" {
        VnodeKind::DevStdio { target_fd: 2 }
    } else if path == b"/dev/fb0" {
        VnodeKind::DevFb { pos: 0 }
    } else if is_tmp_path(path) && path != b"/tmp" {
        // ── Writable /tmp file ────────────────────────────────────────────────
        let writable = flags & (O_WRONLY | O_RDWR) != 0;
        let create   = flags & O_CREAT  != 0;
        let trunc    = flags & O_TRUNC  != 0;

        let mut tmp = TMP_FILES.lock();
        // Look for an existing entry.
        let existing = tmp.iter().position(|e| {
            e.in_use && !e.is_dir && e.path_len == path.len() && &e.path[..path.len()] == path
        });
        match existing {
            Some(idx) => {
                if trunc { tmp[idx].len = 0; }
                let pos = if writable && flags & O_WRONLY != 0 && trunc { 0 }
                          else if flags & O_WRONLY != 0 { tmp[idx].len } // append-style
                          else { 0 };
                VnodeKind::TmpFile { idx, pos, writable }
            }
            None if create => {
                // Allocate a new slot.
                match tmp.iter().position(|e| !e.in_use) {
                    Some(idx) => {
                        tmp[idx] = TmpFileEntry::empty();
                        tmp[idx].in_use   = true;
                        tmp[idx].is_dir   = false;
                        let copy_len = path.len().min(MAX_TMP_PATH - 1);
                        tmp[idx].path[..copy_len].copy_from_slice(&path[..copy_len]);
                        tmp[idx].path_len = copy_len;
                        VnodeKind::TmpFile { idx, pos: 0, writable }
                    }
                    None => return err_reply(-28), // ENOSPC
                }
            }
            None => return err_reply(-2), // ENOENT
        }
    } else if path.starts_with(b"/proc/self/") && path != b"/proc/self/" {
        // ── Dynamic /proc/self/ entries — generated at open time ─────────────────
        let kind = gen_proc_self(pid, path);
        match kind {
            Some(v) => v,
            None    => return err_reply(-2), // ENOENT
        }
    } else if path == b"/proc/meminfo" || path == b"/proc/uptime"
           || path == b"/proc/loadavg" || path == b"/proc/stat"
           || path == b"/proc/self" {
        // ── Dynamic /proc/ system-wide entries ───────────────────────────────────
        match gen_proc_system(path) {
            Some(v) => v,
            None    => return err_reply(-2),
        }
    } else {
        // Check RamFS files first.
        let mut found = None;
        for entry in RAMFS {
            if path == entry.path {
                found = Some(VnodeKind::RamFile { data: entry.data, pos: 0 });
                break;
            }
        }
        if found.is_none() {
            // Check known directories and /tmp subdirs (opened for getdents64).
            for &dir in RAMFS_DIRS {
                if path == dir {
                    found = Some(VnodeKind::RamFile { data: dir, pos: 0 });
                    break;
                }
            }
        }
        if found.is_none() && is_tmp_path(path) {
            // /tmp itself — treat as directory
            found = Some(VnodeKind::RamFile { data: b"/tmp", pos: 0 });
        }
        // Check tmpfs dirs
        if found.is_none() {
            let tmp = TMP_FILES.lock();
            if let Some(_idx) = tmp.iter().position(|e| {
                e.in_use && e.is_dir && e.path_len == path.len() && &e.path[..path.len()] == path
            }) {
                drop(tmp);
                found = Some(VnodeKind::DevNull); // placeholder for empty dir fd
            }
        }
        match found { Some(v) => v, None => return err_reply(-2) }
    };

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
            drop(tbls);
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

            let fb_virt = mm::phys_to_virt(base as usize + cur);
            unsafe {
                core::ptr::copy_nonoverlapping(fb_virt as *const u8, buf, n);
            }
            *pos = cur + n;
            val_reply(n as u64)
        }
        VnodeKind::RamFile { data, pos } => {
            let remaining = data.len().saturating_sub(*pos);
            let n = count.min(remaining).min(4096);
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
                return if r.write_open { err_reply(-11) } else { val_reply(0) };
            }
            let mut n = 0usize;
            while n < count.min(4096) {
                match r.get() { Some(b) => { unsafe { *buf.add(n) = b; } n += 1; } None => break }
            }
            val_reply(n as u64)
        }
        VnodeKind::TmpFile { idx, pos, .. } => {
            let idx = *idx;
            let cur = *pos;
            drop(tbls);
            let mut tmp = TMP_FILES.lock();
            let entry = &mut tmp[idx];
            let remaining = entry.len.saturating_sub(cur);
            let n = count.min(remaining).min(4096);
            if n == 0 { return val_reply(0); }
            unsafe { core::ptr::copy_nonoverlapping(entry.data.as_ptr().add(cur), buf, n); }
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
            let now = sched::ticks();
            let exp = {
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
            };
            if exp == 0 { return err_reply(-11); } // EAGAIN
            TIMERFD_POOL.lock()[slot].expirations = 0;
            unsafe { (buf as *mut u64).write(exp); }
            val_reply(8)
        }
        _ => err_reply(-9),
    }
}

fn handle_write(pid: u32, fd: usize, buf_ptr: usize, count: usize) -> Message {
    if count == 0 { return val_reply(0); }
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    let buf = buf_ptr as *const u8;
    match &mut tbl.fds[fd].kind {
        VnodeKind::DevUrandom | VnodeKind::DevNull | VnodeKind::DevZero =>
            val_reply(count as u64),
        VnodeKind::Pipe { ring, is_write: true } => {
            let ring_idx = *ring;
            drop(tbls);
            let mut rings = PIPE_RINGS.lock();
            let r = &mut rings[ring_idx];
            if !r.read_open { return err_reply(-32); } // EPIPE
            let mut n = 0usize;
            while n < count {
                if !r.put(unsafe { *buf.add(n) }) { break; }
                n += 1;
            }
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
            let entry = &mut tmp[idx];
            let avail = MAX_TMP_SIZE.saturating_sub(cur);
            let n = count.min(avail);
            if n == 0 { return err_reply(-28); } // ENOSPC
            unsafe { core::ptr::copy_nonoverlapping(buf, entry.data.as_mut_ptr().add(cur), n); }
            let new_pos = cur + n;
            if new_pos > entry.len { entry.len = new_pos; }
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
            val_reply(8)
        }
        VnodeKind::DevStdio { target_fd } => {
            let tfd = *target_fd;
            drop(tbls);
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

            let fb_virt = mm::phys_to_virt(base as usize + cur);
            // We use the user-provided buf_ptr directly because we are in the
            // syscall context of the caller (lower-half mapping is active).
            unsafe {
                core::ptr::copy_nonoverlapping(buf, fb_virt as *mut u8, n);
            }
            *pos = cur + n;
            val_reply(n as u64)
        }
        _ => err_reply(-9),
    }
}

fn handle_close(pid: u32, fd: usize) -> Message {
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    match tbl.fds[fd].kind {
        VnodeKind::Pipe { ring, is_write } => {
            drop(tbls);
            let mut rings = PIPE_RINGS.lock();
            if is_write { rings[ring].write_open = false; }
            else        { rings[ring].read_open  = false; }
            return ok_reply();
        }
        VnodeKind::EventFd { slot } => {
            tbl.fds[fd] = FdEntry::empty();
            drop(tbls);
            EVENTFD_COUNTERS.lock()[slot] = u64::MAX;
            return ok_reply();
        }
        VnodeKind::TimerFd { slot } => {
            tbl.fds[fd] = FdEntry::empty();
            drop(tbls);
            TIMERFD_POOL.lock()[slot] = TimerFdEntry::free();
            return ok_reply();
        }
        _ => {}
    }
    tbl.fds[fd] = FdEntry::empty();
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
        VnodeKind::RamFile { data, pos } => {
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
        _ => err_reply(-29), // ESPIPE — not seekable (pipes, devnull, etc.)
    }
}

fn handle_pipe(pid: u32, rfd_ptr: usize, wfd_ptr: usize) -> Message {
    let ring_idx = {
        let mut rings = PIPE_RINGS.lock();
        let mut found = None;
        for (i, r) in rings.iter().enumerate() {
            if !r.read_open && !r.write_open && r.count == 0 {
                found = Some(i); break;
            }
        }
        let i = match found { Some(i) => i, None => return err_reply(-23) };
        rings[i].read_open  = true;
        rings[i].write_open = true;
        i
    };
    let mut tbls = FD_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-12) };
    let rfd = match tbl.alloc_fd() { Some(f) => f, None => return err_reply(-24) };
    tbl.fds[rfd] = FdEntry { kind: VnodeKind::Pipe { ring: ring_idx, is_write: false },
                             flags: 0, in_use: true };
    let wfd = match tbl.alloc_fd() { Some(f) => f, None => {
        tbl.fds[rfd] = FdEntry::empty(); return err_reply(-24);
    }};
    tbl.fds[wfd] = FdEntry { kind: VnodeKind::Pipe { ring: ring_idx, is_write: true },
                             flags: 0, in_use: true };
    unsafe {
        core::ptr::write(rfd_ptr as *mut u32, rfd as u32);
        core::ptr::write(wfd_ptr as *mut u32, wfd as u32);
    }
    ok_reply()
}

fn handle_dup2(pid: u32, oldfd: usize, newfd: usize) -> Message {
    if oldfd >= MAX_FDS || newfd >= MAX_FDS { return err_reply(-9); }
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds[oldfd].in_use { return err_reply(-9); }
    tbl.fds[newfd] = tbl.fds[oldfd];
    val_reply(newfd as u64)
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
    ok_reply()
}

fn handle_exec_cloexec(pid: u32) -> Message {
    let mut tbls = FD_TABLES.lock();
    if let Some(t) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        for fd in t.fds.iter_mut() {
            if fd.in_use && fd.flags & O_CLOEXEC != 0 {
                *fd = FdEntry::empty();
            }
        }
    }
    ok_reply()
}

fn handle_close_all(pid: u32) -> Message {
    let mut tbls = FD_TABLES.lock();
    if let Some(t) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        *t = ProcFdTable::empty();
    }
    ok_reply()
}

fn handle_fcntl(pid: u32, fd: usize, cmd: usize, arg: usize) -> Message {
    // F_GETFD=1, F_SETFD=2, F_GETFL=3, F_SETFL=4
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    match cmd {
        F_GETFD => val_reply((tbl.fds[fd].flags & O_CLOEXEC != 0) as u64),
        F_SETFD => { tbl.fds[fd].flags = arg as u32; ok_reply() }
        F_GETFL => val_reply(tbl.fds[fd].flags as u64),
        F_SETFL => { tbl.fds[fd].flags = (tbl.fds[fd].flags & O_CLOEXEC) | arg as u32; ok_reply() }
        _ => ok_reply(), // silently ignore unknown fcntl
    }
}

/// Allocate a new fd number pointing at the same vnode as `oldfd`.
/// Used by sys_dup() which doesn't know the new fd number in advance.
fn handle_alloc_fd(pid: u32, oldfd: usize) -> Message {
    if oldfd >= MAX_FDS { return err_reply(-9); }
    let mut tbls = FD_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-12) };
    if !tbl.fds[oldfd].in_use { return err_reply(-9); }
    // Find an unused fd > oldfd (POSIX dup() picks lowest available).
    let newfd = match tbl.fds.iter().enumerate()
                    .find(|(i, f)| *i != oldfd && !f.in_use)
                    .map(|(i, _)| i) {
        Some(f) => f, None => return err_reply(-24) // EMFILE
    };
    tbl.fds[newfd] = tbl.fds[oldfd];
    tbl.fds[newfd].flags &= !O_CLOEXEC; // dup() clears O_CLOEXEC
    val_reply(newfd as u64)
}

/// getdents64 — fill `buf` with `struct linux_dirent64` entries for `fd`.
fn handle_getdents64(pid: u32, fd: usize, buf_ptr: usize, count: usize) -> Message {
    if count < 64 { return err_reply(-22); }

    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }

    let (dir_path, start_pos) = match &tbl.fds[fd].kind {
        VnodeKind::RamFile { data, pos } => (*data, *pos),
        _ => return err_reply(-20), // ENOTDIR
    };
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
        else { tbl.fds[fd].kind = VnodeKind::RamFile { data: dir_path, pos }; return val_reply(off as u64); }
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
                    tbl.fds[fd].kind = VnodeKind::RamFile { data: dir_path, pos };
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
                    tbl.fds[fd].kind = VnodeKind::RamFile { data: dir_path, pos };
                    return val_reply(off as u64);
                }
            }
            virtual_idx += 1;
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
                            tbl.fds[fd].kind = VnodeKind::RamFile { data: dir_path, pos };
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

    tbl.fds[fd].kind = VnodeKind::RamFile { data: dir_path, pos };
    val_reply(off as u64)
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
    let mut len = 0;
    
    let mut src = if path.starts_with(b"./") { &path[2..] } else { path };
    if src.starts_with(b"/") { src = &src[1..]; }

    // Convert to absolute for comparison with RAMFS
    abs_path[0] = b'/';
    len = 1;
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

static SERVER_PORT_ID: atomic::AtomicU32 = atomic::AtomicU32::new(u32::MAX);

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
    let fd = match tbl.fds.iter().position(|e| !e.in_use) {
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
    let fd = match tbl.fds.iter().position(|e| !e.in_use) {
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
    if cmd == FBIOGET_VSCREENINFO {
        let mut tbls = FD_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
        if let VnodeKind::DevFb { .. } = &tbl.fds[fd].kind {
            let width  = FB_WIDTH.load(atomic::Ordering::SeqCst);
            let height = FB_HEIGHT.load(atomic::Ordering::SeqCst);
            drop(tbls);
            unsafe {
                let p = arg as *mut u32;
                p.write(width);     // xres
                p.add(1).write(height); // yres
                p.add(2).write(width);  // xres_virtual
                p.add(3).write(height); // yres_virtual
                p.add(4).write(0);      // xoffset
                p.add(5).write(0);      // yoffset
                p.add(6).write(32);     // bits_per_pixel
            }
            return ok_reply();
        }
    }
    if cmd != FIONREAD { return err_reply(-25); }
    if arg == 0 { return err_reply(-14); }
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    let bytes_avail: i32 = match &tbl.fds[fd].kind {
        VnodeKind::Pipe { ring, is_write: false } => { let r = *ring; drop(tbls); PIPE_RINGS.lock()[r].count as i32 }
        VnodeKind::RamFile { data, pos } => (data.len().saturating_sub(*pos)) as i32,
        VnodeKind::TmpFile { idx, pos, .. } => { let i = *idx; let c = *pos; drop(tbls); TMP_FILES.lock()[i].len.saturating_sub(c) as i32 }
        VnodeKind::EventFd { slot } => { let s = *slot; drop(tbls); if EVENTFD_COUNTERS.lock()[s] > 0 { 8 } else { 0 } }
        VnodeKind::TimerFd { slot } => { let s = *slot; drop(tbls); if TIMERFD_POOL.lock()[s].expirations > 0 { 8 } else { 0 } }
        _ => return err_reply(-25),
    };
    unsafe { (arg as *mut i32).write(bytes_avail); }
    val_reply(0)
}

fn handle_ftruncate(pid: u32, fd: usize, new_len: usize) -> Message {
    let mut tbls = FD_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => {
            drop(tbls);
            let mut tmp = TMP_FILES.lock();
            let entry = &mut tmp[idx];
            if new_len > MAX_TMP_SIZE { return err_reply(-28); }
            if new_len > entry.len { for b in &mut entry.data[entry.len..new_len] { *b = 0; } }
            entry.len = new_len;
            ok_reply()
        }
        _ => err_reply(-22),
    }
}

fn handle_rename(old_ptr: usize, new_ptr: usize) -> Message {
    let (obuf, olen) = match read_cstr_raw(old_ptr) { Some(r) => r, None => return err_reply(-14) };
    let (nbuf, nlen) = match read_cstr_raw(new_ptr) { Some(r) => r, None => return err_reply(-14) };
    let old = &obuf[..olen]; let new = &nbuf[..nlen];
    if !is_tmp_path(old) || !is_tmp_path(new) { return err_reply(-30); }
    let mut tmp = TMP_FILES.lock();
    match tmp.iter().position(|e| e.in_use && !e.is_dir && e.path_len == olen && &e.path[..olen] == old) {
        Some(idx) => {
            let copy_len = nlen.min(MAX_TMP_PATH - 1);
            tmp[idx].path[..copy_len].copy_from_slice(&new[..copy_len]);
            tmp[idx].path_len = copy_len;
            ok_reply()
        }
        None => err_reply(-2),
    }
}

fn handle_unlink(path_ptr: usize) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let path = &pbuf[..plen];
    if !is_tmp_path(path) { return err_reply(-30); }
    let mut tmp = TMP_FILES.lock();
    match tmp.iter().position(|e| e.in_use && !e.is_dir && e.path_len == plen && &e.path[..plen] == path) {
        Some(idx) => { tmp[idx] = TmpFileEntry::empty(); ok_reply() }
        None      => err_reply(-2),
    }
}

fn handle_mkdir(path_ptr: usize) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let path = &pbuf[..plen];
    if !is_tmp_path(path) { return err_reply(-30); }
    for &dir in RAMFS_DIRS { if path == dir { return err_reply(-17); } }
    let mut tmp = TMP_FILES.lock();
    if tmp.iter().any(|e| e.in_use && e.path_len == plen && &e.path[..plen] == path) { return err_reply(-17); }
    match tmp.iter().position(|e| !e.in_use) {
        Some(idx) => {
            tmp[idx] = TmpFileEntry::empty(); tmp[idx].in_use = true; tmp[idx].is_dir = true;
            let copy_len = plen.min(MAX_TMP_PATH - 1);
            tmp[idx].path[..copy_len].copy_from_slice(&path[..copy_len]);
            tmp[idx].path_len = copy_len;
            ok_reply()
        }
        None => err_reply(-28),
    }
}

enum FdInfo { Static(&'static [u8]), Pipe(usize), RamData(*const u8), TmpIdx(usize), Bad }

fn handle_fd_path(pid: u32, fd: usize, buf_ptr: usize, buf_len: usize) -> Message {
    if buf_ptr == 0 || buf_len == 0 { return err_reply(-14); }
    let info = {
        let tbls = FD_TABLES.lock();
        let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) { Some(t) => t, None => return err_reply(-9) };
        if fd >= MAX_FDS || !tbl.fds[fd].in_use { return err_reply(-9); }
        match &tbl.fds[fd].kind {
            VnodeKind::DevNull => FdInfo::Static(b"/dev/null"),
            VnodeKind::DevZero => FdInfo::Static(b"/dev/zero"),
            VnodeKind::Pipe { ring, .. } => FdInfo::Pipe(*ring),
            VnodeKind::RamFile { data, .. } => FdInfo::RamData(data.as_ptr()),
            VnodeKind::TmpFile { idx, .. } => FdInfo::TmpIdx(*idx),
            VnodeKind::EventFd { .. } => FdInfo::Static(b"eventfd"),
            VnodeKind::TimerFd { .. } => FdInfo::Static(b"timerfd"),
            VnodeKind::DevUrandom => FdInfo::Static(b"/dev/urandom"),
            VnodeKind::DevStdio { target_fd: 0 } => FdInfo::Static(b"/dev/stdin"),
            VnodeKind::DevStdio { target_fd: 1 } => FdInfo::Static(b"/dev/stdout"),
            VnodeKind::DevStdio { .. } => FdInfo::Static(b"/dev/stderr"),
            _ => FdInfo::Bad,
        }
    };
    match info {
        FdInfo::Bad => err_reply(-9),
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
