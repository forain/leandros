//! A block device (a partition node such as `/dev/sda1`) as a `Read + Write +
//! Seek` stream for the `fatfs` crate, with a 4 KiB block cache.
//!
//! Why a cache: fatfs reads and writes 32-byte directory entries and 4-byte
//! FAT entries one at a time. Without a cache every one is a device command,
//! and the VFS turns each partial-block write into a read-modify-write of a
//! whole 4 KiB block; on a cheap USB stick a kernel copy would take minutes.
//!
//! Writes are write-back: dirty blocks stay in memory until [`Dev::sync`],
//! which writes them in ascending block order (contiguous runs in one call)
//! and then fsyncs the node. The order in which blocks reach the medium is
//! therefore NOT the order fatfs issued them in, so callers that need an
//! on-disk ordering (see `slots::commit_conf`) put a `sync` between the steps.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::rc::Rc;

pub const BS: u64 = 4096;
/// Blocks fetched per cache miss (64 KiB): FAT and directory walks are
/// sequential, and one USB command per 4 KiB would dominate.
const READAHEAD: u64 = 16;
/// Dirty blocks held before an early write-out (64 MiB).
const DIRTY_LIMIT: usize = 16384;
/// Clean blocks kept after a sync (32 MiB); beyond that the cache is dropped.
const CLEAN_LIMIT: usize = 8192;

struct Block {
    data: Box<[u8]>,
    dirty: bool,
}

struct Inner {
    file: File,
    size: u64,
    writable: bool,
    blocks: HashMap<u64, Block>,
    ndirty: usize,
}

/// A handle on the shared device; fatfs owns one, the caller keeps another
/// for `sync`/`invalidate`. Each handle has its own position.
pub struct Dev {
    inner: Rc<RefCell<Inner>>,
    pos: u64,
}

fn dev_size(f: &mut File) -> io::Result<u64> {
    // BLKGETSIZE64 first (what Linux tools use), then the seek end.
    #[cfg(target_os = "linux")]
    {
        let mut sz: u64 = 0;
        const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
        let r = unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64 as _, &mut sz as *mut u64) };
        if r == 0 && sz > 0 {
            return Ok(sz);
        }
    }
    let end = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(0))?;
    if end == 0 {
        return Err(io::Error::new(io::ErrorKind::Other, "cannot determine the device size"));
    }
    Ok(end)
}

impl Dev {
    pub fn open(path: &str, writable: bool) -> io::Result<Dev> {
        let mut file = OpenOptions::new().read(true).write(writable).open(path)?;
        let size = dev_size(&mut file)?;
        Ok(Dev {
            inner: Rc::new(RefCell::new(Inner { file, size, writable, blocks: HashMap::new(), ndirty: 0 })),
            pos: 0,
        })
    }

    pub fn handle(&self) -> Dev {
        Dev { inner: self.inner.clone(), pos: 0 }
    }

    /// Write every dirty block out (ascending order) and fsync the node.
    pub fn sync(&self) -> io::Result<()> {
        let mut g = self.inner.borrow_mut();
        g.write_out()?;
        let fd = g.file.as_raw_fd();
        if unsafe { libc::fsync(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if g.blocks.len() > CLEAN_LIMIT {
            g.blocks.clear();
        }
        Ok(())
    }

    /// Forget every cached block so the next reads come from the medium.
    /// Dirty blocks are an error: `sync` first.
    pub fn invalidate(&self) -> io::Result<()> {
        let mut g = self.inner.borrow_mut();
        if g.ndirty != 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "invalidate with dirty blocks"));
        }
        g.blocks.clear();
        Ok(())
    }
}

impl Inner {
    fn pread(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(off))?;
        let mut done = 0;
        while done < buf.len() {
            let n = self.file.read(&mut buf[done..])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read from the device"));
            }
            done += n;
        }
        Ok(())
    }

    fn pwrite(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(off))?;
        self.file.write_all(buf)
    }

    /// Valid bytes of block `blk` (the last block of an odd-sized device is short).
    fn blk_len(&self, blk: u64) -> usize {
        (self.size - blk * BS).min(BS) as usize
    }

    fn ensure(&mut self, blk: u64) -> io::Result<()> {
        if self.blocks.contains_key(&blk) {
            return Ok(());
        }
        let nblk = (self.size + BS - 1) / BS;
        let mut end = (blk + READAHEAD).min(nblk);
        // Stop the read-ahead at the first block already cached (it may be dirty).
        for b in blk + 1..end {
            if self.blocks.contains_key(&b) {
                end = b;
                break;
            }
        }
        let bytes = ((end - blk) * BS).min(self.size - blk * BS) as usize;
        let mut buf = vec![0u8; bytes];
        self.pread(blk * BS, &mut buf)?;
        for b in blk..end {
            let s = ((b - blk) * BS) as usize;
            let e = (s + BS as usize).min(bytes);
            let mut data = vec![0u8; BS as usize].into_boxed_slice();
            data[..e - s].copy_from_slice(&buf[s..e]);
            self.blocks.insert(b, Block { data, dirty: false });
        }
        Ok(())
    }

    fn write_out(&mut self) -> io::Result<()> {
        if self.ndirty == 0 {
            return Ok(());
        }
        let mut dirty: Vec<u64> = self.blocks.iter().filter(|(_, b)| b.dirty).map(|(k, _)| *k).collect();
        dirty.sort_unstable();
        let mut i = 0;
        while i < dirty.len() {
            // A contiguous run, capped at 256 KiB (the VFS's largest transfer).
            let mut j = i + 1;
            while j < dirty.len() && dirty[j] == dirty[j - 1] + 1 && j - i < 64 {
                j += 1;
            }
            let first = dirty[i];
            let mut buf = Vec::with_capacity((j - i) * BS as usize);
            for b in &dirty[i..j] {
                let len = self.blk_len(*b);
                buf.extend_from_slice(&self.blocks[b].data[..len]);
            }
            self.pwrite(first * BS, &buf)?;
            for b in &dirty[i..j] {
                self.blocks.get_mut(b).unwrap().dirty = false;
            }
            i = j;
        }
        self.ndirty = 0;
        Ok(())
    }
}

impl Read for Dev {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut g = self.inner.borrow_mut();
        if self.pos >= g.size || buf.is_empty() {
            return Ok(0);
        }
        let mut done = 0usize;
        let want = buf.len().min((g.size - self.pos) as usize);
        while done < want {
            let abs = self.pos + done as u64;
            let blk = abs / BS;
            let within = (abs % BS) as usize;
            g.ensure(blk)?;
            let n = (BS as usize - within).min(want - done);
            buf[done..done + n].copy_from_slice(&g.blocks[&blk].data[within..within + n]);
            done += n;
        }
        self.pos += done as u64;
        Ok(done)
    }
}

impl Write for Dev {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut g = self.inner.borrow_mut();
        if !g.writable {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "device opened read-only"));
        }
        if self.pos >= g.size {
            return Err(io::Error::new(io::ErrorKind::Other, "write past the end of the device"));
        }
        let want = buf.len().min((g.size - self.pos) as usize);
        let mut done = 0usize;
        while done < want {
            let abs = self.pos + done as u64;
            let blk = abs / BS;
            let within = (abs % BS) as usize;
            let n = (BS as usize - within).min(want - done);
            if within == 0 && n == g.blk_len(blk) && !g.blocks.contains_key(&blk) {
                // Whole block: no need to read the old contents.
                let mut data = vec![0u8; BS as usize].into_boxed_slice();
                data[..n].copy_from_slice(&buf[done..done + n]);
                g.blocks.insert(blk, Block { data, dirty: true });
                g.ndirty += 1;
            } else {
                g.ensure(blk)?;
                let b = g.blocks.get_mut(&blk).unwrap();
                b.data[within..within + n].copy_from_slice(&buf[done..done + n]);
                if !b.dirty {
                    b.dirty = true;
                    g.ndirty += 1;
                }
            }
            done += n;
        }
        self.pos += done as u64;
        if g.ndirty > DIRTY_LIMIT {
            g.write_out()?;
        }
        Ok(done)
    }

    fn flush(&mut self) -> io::Result<()> {
        // fatfs calls this on unmount; the ordered write-out is `Dev::sync`.
        Ok(())
    }
}

impl Seek for Dev {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let size = self.inner.borrow().size as i64;
        let new = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => size + d,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}
