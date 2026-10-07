//! Backend for `drivers::firmware` (`request_firmware`): read a file by
//! absolute path from the real root filesystem, from kernel context.
//!
//! It talks to the f2fs mount that `pivot_root` made "/" directly, the way
//! demand-paged exec does: open through the mount's port (`vfs::call_port`,
//! a synchronous in-kernel call), `pread_by_port` into a 1 MiB kernel bounce
//! buffer, `close_by_port`. No fd table is touched (a kernel thread would
//! otherwise claim an FD_TABLES slot for good, and a user ioctl would see a
//! stray fd), no user memory is involved, and paths are not resolved against
//! the caller's cwd/chroot. Credentials are the current task's (root in a
//! kernel thread). `F2FS_MOUNTS` is held per chunk only; between chunks a
//! kernel thread gives the CPU a chance (`cond_resched`).

use ipc::Message;
use vfs_server as vfs;

const ENOENT: i32 = -2;
const EIO: i32 = -5;
const ENODEV: i32 = -19;
const ENAMETOOLONG: i32 = -36;
const CHUNK: usize = 1 << 20;

fn reply_val(m: &Message) -> i64 { i64::from_le_bytes(m.data[0..8].try_into().unwrap_or([0; 8])) }

/// The f2fs port mounted at "/" (after init's pivot_root), if any.
fn root_port() -> Option<u32> {
    vfs::list_mounts().iter()
        .find(|m| m.in_use && m.prefix == "/" && m.fstype == "f2fs")
        .map(|m| m.port)
}

struct FwFile { port: u32, id: u64 }

impl Drop for FwFile {
    fn drop(&mut self) { f2fs_server::close_by_port(self.port, self.id) }
}

fn open(path: &str) -> Result<FwFile, i32> {
    let port = root_port().ok_or(ENODEV)?;
    if !path.starts_with('/') { return Err(ENOENT); }
    if path.len() >= 255 { return Err(ENAMETOOLONG); }
    let mut p = [0u8; 256];
    p[..path.len()].copy_from_slice(path.as_bytes());
    let mut m = Message::empty();
    m.tag = vfs::VFS_OPEN;
    m.data[0..8].copy_from_slice(&(p.as_ptr() as u64).to_le_bytes());
    // data[8..16] = 0 (O_RDONLY), data[16..24] = 0 (mode)
    let r = reply_val(&vfs::call_port(port, m));
    if r < 0 { Err(r as i32) } else { Ok(FwFile { port, id: r as u64 }) }
}

fn size_of(f: &FwFile) -> Result<u64, i32> {
    let mut m = Message::empty();
    m.tag = vfs::VFS_LSEEK;
    m.data[0..8].copy_from_slice(&f.id.to_le_bytes());
    m.data[16..24].copy_from_slice(&2u64.to_le_bytes()); // SEEK_END, offset 0
    let r = reply_val(&vfs::call_port(f.port, m));
    if r < 0 { Err(r as i32) } else { Ok(r as u64) }
}

fn root_ready() -> bool { root_port().is_some() }

fn size(path: &str) -> Result<u64, i32> { size_of(&open(path)?) }

fn read(path: &str, sink: &mut dyn FnMut(u64, &[u8]) -> bool) -> Result<u64, i32> {
    let fh = open(path)?;
    let total = size_of(&fh)?;
    let mut buf = alloc::vec::Vec::new();
    if buf.try_reserve_exact(CHUNK.min(total.max(1) as usize)).is_err() { return Err(-12); }
    buf.resize(CHUNK.min(total.max(1) as usize), 0u8);
    let mut off = 0u64;
    while off < total {
        let want = ((total - off) as usize).min(buf.len());
        let n = f2fs_server::pread_by_port(fh.port, fh.id, buf.as_mut_ptr(), want, off);
        if n < 0 { return Err(n as i32); }
        if n == 0 { return Err(EIO); }
        if !sink(off, &buf[..n as usize]) { return Ok(off + n as u64); }
        off += n as u64;
        sched::kthread::cond_resched();
    }
    Ok(total)
}

static BACKEND: drivers::firmware::FirmwareBackend = drivers::firmware::FirmwareBackend {
    root_ready,
    size,
    read,
};

/// Register with `drivers::firmware` (kernel init).
pub fn init() {
    drivers::firmware::set_backend(&BACKEND);
}
