//! Root block copy: f2fs superblock, remount ro/rw, zero-skipping copy with
//! a running SHA-256 of the source extent, and read-back verification.

use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};

pub const BLK: usize = 4096;
const CHUNK: usize = 1 << 20;
const F2FS_MAGIC: u32 = 0xF2F5_2010;

#[derive(Clone, Copy, Debug)]
pub struct Sb {
    pub block_count: u64,
    pub main_blkaddr: u32,
}

impl Sb {
    pub fn bytes(&self) -> u64 {
        self.block_count * BLK as u64
    }
}

/// Parse the f2fs superblock out of the first 4 KiB..8 KiB of a partition.
pub fn parse_sb(raw: &[u8]) -> Result<Sb, String> {
    if raw.len() < 1024 + 96 {
        return Err("short superblock read".into());
    }
    let sb = &raw[1024..];
    let magic = u32::from_le_bytes(sb[0..4].try_into().unwrap());
    if magic != F2FS_MAGIC {
        return Err(format!("not an f2fs volume (magic {:#x})", magic));
    }
    let log_blocksize = u32::from_le_bytes(sb[16..20].try_into().unwrap());
    if log_blocksize != 12 {
        return Err(format!("unsupported f2fs block size 2^{}", log_blocksize));
    }
    Ok(Sb {
        block_count: u64::from_le_bytes(sb[36..44].try_into().unwrap()),
        main_blkaddr: u32::from_le_bytes(sb[92..96].try_into().unwrap()),
    })
}

pub fn read_sb(disk_dev: &str, part_off: u64) -> io::Result<Sb> {
    let mut f = File::open(disk_dev)?;
    f.seek(SeekFrom::Start(part_off))?;
    let mut raw = vec![0u8; 8192];
    f.read_exact(&mut raw)?;
    parse_sb(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ── remount ──────────────────────────────────────────────────────────────

static RO: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "linux")]
fn do_remount(ro: bool) -> io::Result<()> {
    let flags = libc::MS_REMOUNT | if ro { libc::MS_RDONLY } else { 0 };
    let root = b"/\0";
    let r = unsafe { libc::mount(std::ptr::null(), root.as_ptr() as *const libc::c_char, std::ptr::null(), flags, std::ptr::null()) };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(target_os = "linux"))]
fn do_remount(_ro: bool) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Other, "remount needs Linux"))
}

#[cfg(target_os = "linux")]
extern "C" fn on_signal(_sig: libc::c_int) {
    if RO.load(Ordering::SeqCst) {
        let _ = do_remount(false);
    }
    unsafe { libc::_exit(130) };
}

/// Remounts `/` read-only on creation and read-write on drop or on a
/// SIGINT/SIGTERM/SIGHUP.
pub struct RoGuard {
    active: bool,
}

impl RoGuard {
    pub fn new() -> io::Result<RoGuard> {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::sync();
            for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::signal(s, on_signal as extern "C" fn(libc::c_int) as usize);
            }
        }
        do_remount(true)?;
        RO.store(true, Ordering::SeqCst);
        Ok(RoGuard { active: true })
    }

    pub fn release(&mut self) -> io::Result<()> {
        if self.active {
            self.active = false;
            RO.store(false, Ordering::SeqCst);
            do_remount(false)?;
        }
        Ok(())
    }
}

impl Drop for RoGuard {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

// ── copy ─────────────────────────────────────────────────────────────────

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<()> {
    f.read_exact(buf)
}

fn is_zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

pub struct CopyStats {
    pub sha256: [u8; 32],
    pub written: u64,
    pub skipped: u64,
}

/// Copy `sb.bytes()` from `src` (at `src_off`) to `dst` (at `dst_off`). The
/// metadata area `[0, main_blkaddr)` is always written; in the main area a
/// zero source block is skipped when the target block is already zero (and
/// rewritten as zero when it is not). `zero_fill` writes everything.
/// `progress(pct)` is called when the percentage changes.
pub fn copy_root(
    src_dev: &str,
    src_off: u64,
    dst_dev: &str,
    dst_off: u64,
    sb: &Sb,
    zero_fill: bool,
    mut progress: impl FnMut(u32),
) -> io::Result<CopyStats> {
    let mut src = File::open(src_dev)?;
    let mut dst = OpenOptions::new().read(true).write(true).open(dst_dev)?;
    let total = sb.bytes();
    let meta = sb.main_blkaddr as u64 * BLK as u64;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut tbuf = vec![0u8; CHUNK];
    let (mut written, mut skipped) = (0u64, 0u64);
    let mut off = 0u64;
    let mut last = 101u32;
    src.seek(SeekFrom::Start(src_off))?;
    while off < total {
        let n = ((total - off) as usize).min(CHUNK);
        read_full(&mut src, &mut buf[..n])?;
        h.update(&buf[..n]);
        // Which blocks to write.
        let nb = (n + BLK - 1) / BLK;
        let mut need = vec![true; nb];
        if !zero_fill && off + n as u64 > meta {
            let mut any_zero = false;
            for b in 0..nb {
                let s = b * BLK;
                let e = (s + BLK).min(n);
                if off + s as u64 >= meta && is_zero(&buf[s..e]) {
                    need[b] = false;
                    any_zero = true;
                }
            }
            if any_zero {
                dst.seek(SeekFrom::Start(dst_off + off))?;
                read_full(&mut dst, &mut tbuf[..n])?;
                for b in 0..nb {
                    if !need[b] {
                        let s = b * BLK;
                        let e = (s + BLK).min(n);
                        if !is_zero(&tbuf[s..e]) {
                            need[b] = true; // stale data: overwrite with zeros
                        }
                    }
                }
            }
        }
        // Write contiguous runs.
        let mut b = 0;
        while b < nb {
            if !need[b] {
                skipped += BLK as u64;
                b += 1;
                continue;
            }
            let mut e = b;
            while e < nb && need[e] {
                e += 1;
            }
            let s = b * BLK;
            let end = (e * BLK).min(n);
            dst.seek(SeekFrom::Start(dst_off + off + s as u64))?;
            dst.write_all(&buf[s..end])?;
            written += (end - s) as u64;
            b = e;
        }
        off += n as u64;
        let pct = (off * 100 / total) as u32;
        if pct != last {
            last = pct;
            progress(pct);
        }
    }
    dst.sync_all()?;
    flush_dev(&dst);
    Ok(CopyStats { sha256: h.finalize().into(), written, skipped })
}

/// BLKFLSBUF on an open block device (ignored on failure).
pub fn flush_dev(f: &File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::ioctl(f.as_raw_fd(), 0x1261 as _, 0) };
    }
    let _ = f;
}

/// Re-read the target extent and compare its SHA-256 with `want`.
pub fn verify_root(dst_dev: &str, dst_off: u64, len: u64, want: &[u8; 32], mut progress: impl FnMut(u32)) -> io::Result<()> {
    let mut dst = File::open(dst_dev)?;
    dst.seek(SeekFrom::Start(dst_off))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut off = 0u64;
    let mut last = 101;
    while off < len {
        let n = ((len - off) as usize).min(CHUNK);
        read_full(&mut dst, &mut buf[..n])?;
        h.update(&buf[..n]);
        off += n as u64;
        let pct = (off * 100 / len) as u32;
        if pct != last {
            last = pct;
            progress(pct);
        }
    }
    let got: [u8; 32] = h.finalize().into();
    if &got != want {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "root verification failed: SHA-256 of the copy differs from the source"));
    }
    Ok(())
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}
