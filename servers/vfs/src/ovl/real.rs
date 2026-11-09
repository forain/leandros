//! Real-layer operations on kernel buffers (design-overlay.md section 2.2).
//!
//! Every helper builds a VFS message whose pointers are kernel addresses and
//! feeds it to the normal handlers as a `pid` (normally the per-mount
//! pseudo-pid, see `super::pseudo_pid`). Results are `Result<T, i32>` with a
//! POSITIVE errno on failure. Paths are plain byte slices; a NUL is appended
//! here. Nothing in this file touches the mount table.

use alloc::vec::Vec;
use ipc::Message;

use crate::*;

/// Parsed subset of the Linux `struct stat` the VFS writes.
#[derive(Clone, Copy, Debug, Default)]
pub struct RStat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: i64,
}

pub const STAT_BYTES: usize = 144;

fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn rd64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

#[cfg(target_arch = "aarch64")]
fn parse_stat(b: &[u8; STAT_BYTES]) -> RStat {
    RStat { dev: rd64(b, 0), ino: rd64(b, 8), mode: rd32(b, 16), nlink: rd32(b, 20),
            uid: rd32(b, 24), gid: rd32(b, 28), rdev: rd64(b, 32), size: rd64(b, 48) as i64 }
}
#[cfg(target_arch = "x86_64")]
fn parse_stat(b: &[u8; STAT_BYTES]) -> RStat {
    RStat { dev: rd64(b, 0), ino: rd64(b, 8), nlink: rd64(b, 16) as u32, mode: rd32(b, 24),
            uid: rd32(b, 28), gid: rd32(b, 32), rdev: rd64(b, 40), size: rd64(b, 48) as i64 }
}

/// NUL-terminated copy of `p`, alive for the duration of one call.
fn cz(p: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(p.len() + 1);
    v.extend_from_slice(p);
    v.push(0);
    v
}

fn call(pid: u32, tag: u64, a: &[u64]) -> Result<i64, i32> {
    let mut m = Message::empty();
    m.tag = tag;
    for (i, v) in a.iter().enumerate() {
        m.data[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    let _g = super::enter(pid);
    let r = reply_val(&crate::handle_as(&m, pid));
    if r < 0 { Err((-r) as i32) } else { Ok(r) }
}

fn p(v: &[u8]) -> u64 { v.as_ptr() as u64 }

pub fn lstat(pid: u32, path: &[u8]) -> Result<RStat, i32> {
    let c = cz(path);
    let mut st = [0u8; STAT_BYTES];
    call(pid, VFS_LSTAT, &[p(&c), st.as_mut_ptr() as u64])?;
    Ok(parse_stat(&st))
}

pub fn readlink(pid: u32, path: &[u8]) -> Result<Vec<u8>, i32> {
    let c = cz(path);
    let mut buf = alloc::vec![0u8; 4096];
    let n = call(pid, VFS_READLINK, &[p(&c), buf.as_mut_ptr() as u64, buf.len() as u64])? as usize;
    buf.truncate(n);
    Ok(buf)
}

pub fn open(pid: u32, path: &[u8], flags: u32, mode: u32) -> Result<usize, i32> {
    let c = cz(path);
    call(pid, VFS_OPEN, &[p(&c), flags as u64, mode as u64]).map(|v| v as usize)
}

pub fn close(pid: u32, fd: usize) -> Result<(), i32> {
    call(pid, VFS_CLOSE, &[fd as u64]).map(|_| ())
}

/// Positioned read: VFS_PREAD only serves block devices, so seek + read. The
/// fd belongs to the (private) pseudo-pid table, so there is no shared offset.
pub fn pread(pid: u32, fd: usize, buf: &mut [u8], off: u64) -> Result<usize, i32> {
    call(pid, VFS_LSEEK, &[fd as u64, off, 0])?;
    call(pid, VFS_READ, &[fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64])
        .map(|v| v as usize)
}

pub fn pwrite(pid: u32, fd: usize, buf: &[u8], off: u64) -> Result<usize, i32> {
    call(pid, VFS_LSEEK, &[fd as u64, off, 0])?;
    call(pid, VFS_WRITE, &[fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0])
        .map(|v| v as usize)
}

/// Raw linux_dirent64 stream; returns bytes filled (0 = end).
pub fn getdents(pid: u32, fd: usize, buf: &mut [u8]) -> Result<usize, i32> {
    call(pid, VFS_GETDENTS64, &[fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64])
        .map(|v| v as usize)
}

pub fn mkdir(pid: u32, path: &[u8], mode: u32) -> Result<(), i32> {
    let c = cz(path);
    call(pid, VFS_MKDIR, &[p(&c), mode as u64]).map(|_| ())
}

/// `mode` carries the S_IF* type bits (same packing as sys_mknodat).
pub fn mknod(pid: u32, path: &[u8], mode: u32) -> Result<(), i32> {
    let c = cz(path);
    call(pid, VFS_MKNOD, &[p(&c), mode as u64]).map(|_| ())
}

pub fn symlink(pid: u32, target: &[u8], linkpath: &[u8]) -> Result<(), i32> {
    let t = cz(target);
    let l = cz(linkpath);
    call(pid, VFS_SYMLINK, &[p(&t), p(&l)]).map(|_| ())
}

pub fn link(pid: u32, old: &[u8], new: &[u8]) -> Result<(), i32> {
    let o = cz(old);
    let n = cz(new);
    call(pid, VFS_LINK, &[p(&o), p(&n)]).map(|_| ())
}

pub fn unlink(pid: u32, path: &[u8]) -> Result<(), i32> {
    let c = cz(path);
    call(pid, VFS_UNLINK, &[p(&c)]).map(|_| ())
}

pub fn rmdir(pid: u32, path: &[u8]) -> Result<(), i32> {
    let c = cz(path);
    call(pid, VFS_RMDIR, &[p(&c)]).map(|_| ())
}

/// RENAME_NOREPLACE when `noreplace`, plain rename otherwise.
pub fn rename(pid: u32, old: &[u8], new: &[u8], noreplace: bool) -> Result<(), i32> {
    let o = cz(old);
    let n = cz(new);
    call(pid, VFS_RENAME, &[p(&o), p(&n), if noreplace { 1 } else { 0 }]).map(|_| ())
}

pub fn chmod(pid: u32, path: &[u8], mode: u32) -> Result<(), i32> {
    let c = cz(path);
    call(pid, VFS_LCHMOD, &[p(&c), mode as u64]).map(|_| ())
}

pub fn lchown(pid: u32, path: &[u8], uid: u32, gid: u32) -> Result<(), i32> {
    let c = cz(path);
    call(pid, VFS_LCHOWN, &[p(&c), uid as u64, gid as u64]).map(|_| ())
}

/// Set atime/mtime (sec, nsec each) without following a final symlink.
pub fn utimens(pid: u32, path: &[u8], at: (i64, i64), mt: (i64, i64)) -> Result<(), i32> {
    let c = cz(path);
    call(pid, VFS_LUTIMENS, &[p(&c), at.0 as u64, at.1 as u64, mt.0 as u64, mt.1 as u64,
                              UTIMENS_EXPLICIT]).map(|_| ())
}

pub fn statfs(pid: u32, path: &[u8], out: &mut [u8; 120]) -> Result<(), i32> {
    let c = cz(path);
    call(pid, VFS_STATFS, &[p(&c), out.as_mut_ptr() as u64]).map(|_| ())
}

/// l-variant; returns the value length (copies up to `buf.len()` bytes).
/// An absent name is `Err(ENODATA)`.
pub fn lgetxattr(pid: u32, path: &[u8], name: &[u8], buf: &mut [u8]) -> Result<usize, i32> {
    let c = cz(path);
    let n = cz(name);
    call(pid, VFS_LGETXATTR, &[p(&c), p(&n), buf.as_mut_ptr() as u64, buf.len() as u64])
        .map(|v| v as usize)
}

pub fn lsetxattr(pid: u32, path: &[u8], name: &[u8], val: &[u8], flags: u32) -> Result<(), i32> {
    let c = cz(path);
    let n = cz(name);
    call(pid, VFS_LSETXATTR, &[p(&c), p(&n), p(val), val.len() as u64, flags as u64]).map(|_| ())
}

/// NUL-separated name list; `buf` of length 0 asks for the size only.
pub fn llistxattr(pid: u32, path: &[u8], buf: &mut [u8]) -> Result<usize, i32> {
    let c = cz(path);
    call(pid, VFS_LLISTXATTR, &[p(&c), buf.as_mut_ptr() as u64, buf.len() as u64])
        .map(|v| v as usize)
}

pub fn lremovexattr(pid: u32, path: &[u8], name: &[u8]) -> Result<(), i32> {
    let c = cz(path);
    let n = cz(name);
    call(pid, VFS_LREMOVEXATTR, &[p(&c), p(&n)]).map(|_| ())
}

// ── selftest ────────────────────────────────────────────────────────────────

static NPASS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static NFAIL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Only failures are listed (the /proc buffer is 2 KiB); passes are counted.
fn line(out: &mut Vec<u8>, ok: bool, name: &str, _detail: &str) {
    use core::sync::atomic::Ordering::Relaxed;
    if ok { NPASS.fetch_add(1, Relaxed); return; }
    NFAIL.fetch_add(1, Relaxed);
    out.extend_from_slice(b"FAIL ");
    out.extend_from_slice(name.as_bytes());
    out.push(b'\n');
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    for p in parts { v.extend_from_slice(p); }
    v
}

// Selftest mount ids: far from anything a real overlay mount will use.
const ST_ROOT: u32 = 0xFFF0;
const ST_USER: u32 = 0xFFF1;

fn run_base(out: &mut Vec<u8>, base: &[u8], tag: &str) {
    let rp = super::pseudo_pid(ST_ROOT);
    let up = super::pseudo_pid(ST_USER);
    let dir = cat(&[base, b"/.ovlself"]);
    let file = cat(&[&dir, b"/f"]);
    let sub = cat(&[&dir, b"/d"]);
    let lnk = cat(&[&dir, b"/l"]);
    let mut nm = alloc::string::String::new();
    macro_rules! t {
        ($name:expr, $ok:expr) => {{
            nm.clear(); nm.push_str(tag); nm.push(':'); nm.push_str($name);
            line(out, $ok, &nm, "");
        }};
    }
    // Leftovers from an aborted run.
    let _ = unlink(rp, &file); let _ = unlink(rp, &lnk); let _ = rmdir(rp, &sub);
    let _ = unlink(rp, &cat(&[&dir, b"/g"])); let _ = unlink(rp, &cat(&[&dir, b"/u"]));
    let _ = rmdir(rp, &dir);

    t!("mkdir", mkdir(rp, &dir, 0o755).is_ok());
    t!("mkdir-sub", mkdir(rp, &sub, 0o755).is_ok());
    // data round trip
    let fd = open(rp, &file, 0o2 | 0o100, 0o644);
    t!("open-create", fd.is_ok());
    if let Ok(fd) = fd {
        t!("pwrite", pwrite(rp, fd, b"hello", 0) == Ok(5));
        let mut b = [0u8; 8];
        t!("pread", pread(rp, fd, &mut b, 0) == Ok(5) && &b[..5] == b"hello");
        t!("close", close(rp, fd).is_ok());
    }
    let st = lstat(rp, &file);
    t!("lstat-reg", matches!(st, Ok(s) if s.mode & 0o170000 == 0o100000
        && s.mode & 0o7777 == 0o644 && s.size == 5 && s.uid == 0));
    t!("lstat-dir", matches!(lstat(rp, &sub), Ok(s) if s.mode & 0o170000 == 0o040000));
    t!("lstat-enoent", matches!(lstat(rp, &cat(&[&dir, b"/nope"])), Err(2)));
    // symlink
    t!("symlink", symlink(rp, b"target-x", &lnk).is_ok());
    t!("readlink", readlink(rp, &lnk).as_deref() == Ok(&b"target-x"[..]));
    t!("lstat-lnk", matches!(lstat(rp, &lnk), Ok(s) if s.mode & 0o170000 == 0o120000));
    // chmod / lchown / utimens
    t!("chmod", chmod(rp, &file, 0o600).is_ok()
        && matches!(lstat(rp, &file), Ok(s) if s.mode & 0o7777 == 0o600));
    t!("lchown", lchown(rp, &file, 4242, 4343).is_ok()
        && matches!(lstat(rp, &file), Ok(s) if s.uid == 4242 && s.gid == 4343));
    t!("utimens", utimens(rp, &file, (1000, 0), (2000, 0)).is_ok());
    // getdents sees the entries
    if let Ok(fd) = open(rp, &dir, 0o200000 | 0o400000, 0) {
        let mut b = alloc::vec![0u8; 2048];
        let n = getdents(rp, fd, &mut b).unwrap_or(0);
        let hay = &b[..n];
        let has = |s: &[u8]| hay.windows(s.len()).any(|w| w == s);
        t!("getdents", n > 0 && has(b"f\0") && has(b"d\0") && has(b"l\0"));
        let _ = close(rp, fd);
    } else {
        t!("getdents", false);
    }
    // ── xattr round trip + ENODATA audit (user.* on a file and a dir) ──
    for (what, path) in [("file", &file), ("dir", &sub)] {
        let mut small = [0u8; 16];
        let mut nm2 = alloc::string::String::new();
        nm2.push_str(what);
        let mut ck = |out: &mut Vec<u8>, n: &str, ok: bool| {
            let mut full = alloc::string::String::new();
            full.push_str(tag); full.push_str(":xattr-"); full.push_str(&nm2);
            full.push('-'); full.push_str(n);
            line(out, ok, &full, "");
        };
        ck(out, "get-absent", lgetxattr(rp, path, b"user.nope", &mut small) == Err(61));
        ck(out, "get-absent-probe", lgetxattr(rp, path, b"user.nope", &mut []) == Err(61));
        ck(out, "remove-absent", lremovexattr(rp, path, b"user.nope") == Err(61));
        ck(out, "list-empty", llistxattr(rp, path, &mut []) == Ok(0));
        ck(out, "set", lsetxattr(rp, path, b"user.k", b"vv", 0).is_ok());
        ck(out, "get", lgetxattr(rp, path, b"user.k", &mut small) == Ok(2) && &small[..2] == b"vv");
        ck(out, "list-size", llistxattr(rp, path, &mut []) == Ok(7));
        let mut lb = [0u8; 32];
        ck(out, "list", llistxattr(rp, path, &mut lb) == Ok(7) && &lb[..7] == b"user.k\0");
        ck(out, "get-other-absent", lgetxattr(rp, path, b"user.other", &mut small) == Err(61));
        ck(out, "remove", lremovexattr(rp, path, b"user.k").is_ok());
        ck(out, "get-after-remove", lgetxattr(rp, path, b"user.k", &mut small) == Err(61));
        ck(out, "remove-twice", lremovexattr(rp, path, b"user.k") == Err(61));
        ck(out, "list-after", llistxattr(rp, path, &mut []) == Ok(0));
    }
    // ── credential override: the unprivileged mounter ──
    // /.ovlself is 0755 root: uid 4242 may not create there, but may in 0777.
    let denied = cat(&[&dir, b"/u"]);
    t!("cred-eacces", open(up, &denied, 0o2 | 0o100, 0o644).err() == Some(13));
    t!("cred-chmod-dir", chmod(rp, &sub, 0o777).is_ok());
    let uf = cat(&[&sub, b"/mine"]);
    let ufd = open(up, &uf, 0o2 | 0o100, 0o644);
    t!("cred-create", ufd.is_ok());
    if let Ok(fd) = ufd { let _ = close(up, fd); }
    t!("cred-owner", matches!(lstat(rp, &uf), Ok(s) if s.uid == 4242 && s.gid == 4343));
    let _ = unlink(up, &uf);
    // cleanup
    let _ = unlink(rp, &denied);
    let _ = unlink(rp, &file); let _ = unlink(rp, &lnk);
    let _ = rmdir(rp, &sub);
    t!("rmdir", rmdir(rp, &dir).is_ok());
}

pub fn selftest(out: &mut Vec<u8>) {
    use core::sync::atomic::Ordering::Relaxed;
    NPASS.store(0, Relaxed);
    NFAIL.store(0, Relaxed);
    super::register(ST_ROOT, xattr::Cred::new(0, 0));
    super::register(ST_USER, xattr::Cred::new(4242, 4343));
    run_base(out, b"/tmp", "tmpfs");
    run_base(out, b"/home", "f2fs");
    super::unregister(ST_ROOT);
    super::unregister(ST_USER);
    let mut tail = alloc::format!("RESULT pass={} fail={}\n",
        NPASS.load(Relaxed), NFAIL.load(Relaxed));
    out.extend_from_slice(unsafe { tail.as_bytes_mut() });
}
