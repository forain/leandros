//! The target device: a block device on a running system, or a regular file
//! (including a sparse image) when testing.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// BLKGETSIZE64 — `_IOR(0x12, 114, size_t)`. The request argument is c_int on
/// musl and c_ulong on glibc, so the constant is cast at the call site rather
/// than typed here. (The macOS arm exists only so `cargo test` runs on the
/// build host; LeandrOS itself is always the Linux target.)
const BLKGETSIZE64: u64 = 0x8008_1272;

#[cfg(target_os = "linux")]
type IoctlReq = libc::Ioctl;
#[cfg(not(target_os = "linux"))]
type IoctlReq = libc::c_ulong;

pub struct Dev {
    f: File,
    dry_run: bool,
    /// Bytes actually pushed to the device, for the "stayed sparse" claim.
    written: std::cell::Cell<u64>,
}

impl Dev {
    pub fn open(path: &Path, dry_run: bool) -> io::Result<Dev> {
        let f = OpenOptions::new().read(true).write(!dry_run).open(path)?;
        Ok(Dev {
            f,
            dry_run,
            written: std::cell::Cell::new(0),
        })
    }

    /// Size in bytes. `lseek(SEEK_END)` answers for regular files and, on
    /// Linux, for block devices too; the BLKGETSIZE64 ioctl is the fallback for
    /// devices that report zero.
    pub fn size(&mut self) -> io::Result<u64> {
        let by_seek = self.f.seek(SeekFrom::End(0))?;
        self.f.seek(SeekFrom::Start(0))?;
        if by_seek > 0 {
            return Ok(by_seek);
        }
        let mut sz: u64 = 0;
        let rc = unsafe {
            libc::ioctl(self.f.as_raw_fd(), BLKGETSIZE64 as IoctlReq, &mut sz)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sz)
    }

    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_at_best_effort(&self.f, off, buf)
    }

    pub fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        self.f.write_all_at(buf, off)?;
        self.written.set(self.written.get() + buf.len() as u64);
        Ok(())
    }

    /// Make `[off, off+len)` read as zeroes, without materialising blocks that
    /// already are. Reads the range a megabyte at a time and only writes back
    /// the chunks that contain something. On a fresh sparse image this writes
    /// nothing at all; on a device carrying an old filesystem it wipes it.
    pub fn zero_range(&self, off: u64, len: u64) -> io::Result<()> {
        if self.dry_run || len == 0 {
            return Ok(());
        }
        const CHUNK: usize = 1 << 20;
        let zeros = vec![0u8; CHUNK];
        let mut buf = vec![0u8; CHUNK];
        let mut p = 0u64;
        while p < len {
            let n = (len - p).min(CHUNK as u64) as usize;
            let got = read_at_best_effort(&self.f, off + p, &mut buf[..n])?;
            let dirty = buf[..got].iter().any(|&b| b != 0);
            if dirty {
                self.f.write_all_at(&zeros[..n], off + p)?;
                self.written.set(self.written.get() + n as u64);
            }
            p += n as u64;
        }
        Ok(())
    }

    pub fn sync(&self) -> io::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        self.f.sync_all()
    }

    pub fn bytes_written(&self) -> u64 {
        self.written.get()
    }
}

/// Read as much of `buf` as the file has; a short read at EOF is not an error,
/// the tail simply reads as absent (and therefore as zero).
fn read_at_best_effort(f: &File, off: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut done = 0usize;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], off + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}
