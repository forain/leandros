//! LeandrOS shrink guard: a private INCOMPAT bit (0x80000000) in the primary
//! and every backup superblock of the ext4 filesystem being shrunk.
//!
//! Stock Linux (kernel ext4, e2fsck, resize2fs, tune2fs) refuses a filesystem
//! with an unknown INCOMPAT bit before it writes anything, so a half-resized
//! filesystem cannot be touched by Linux after a power loss. Our e2fsprogs
//! port (ports/e2fsprogs/patches/leandros-guard.patch) accepts the bit.
//!
//! Only `s_feature_incompat` (0x60) and `s_checksum` (0x3FC) are edited. Each
//! copy is checked to look like the superblock of this filesystem (magic,
//! group number, UUID, checksum) before it is written. Order: backups first,
//! the primary last, in both directions. See artifacts/orch/linuxguard-design.md.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;

pub const GUARD: u32 = 0x8000_0000;
const METADATA_CSUM: u32 = 0x400;

pub fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
        }
    }
    crc
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

#[derive(Debug, Clone)]
pub struct Copy {
    pub group: u64,
    pub off: u64,
}

fn csum_on(b: &[u8]) -> bool {
    le32(b, 0x64) & METADATA_CSUM != 0
}

fn csum_ok(b: &[u8]) -> bool {
    !csum_on(b) || crc32c(!0, &b[..0x3FC]) == le32(b, 0x3FC)
}

/// Where the superblock copies are, derived from a primary superblock:
/// backups in ascending group order, the primary (group 0) last.
pub fn copies(p: &[u8]) -> Result<Vec<Copy>, String> {
    if p.len() < 1024 || u16::from_le_bytes([p[0x38], p[0x39]]) != 0xEF53 {
        return Err("primary superblock has no 0xEF53 magic".into());
    }
    let incompat = le32(p, 0x60);
    let blocks = le32(p, 0x04) as u64 | if incompat & 0x80 != 0 { (le32(p, 0x150) as u64) << 32 } else { 0 };
    let bpg = le32(p, 0x20) as u64;
    let fdb = le32(p, 0x14) as u64;
    let bs = 1024u64 << le32(p, 0x18);
    if bpg == 0 || blocks <= fdb {
        return Err("implausible superblock geometry".into());
    }
    let groups = (blocks - fdb + bpg - 1) / bpg;
    let compat = le32(p, 0x5C);
    let ro = le32(p, 0x64);
    let mut gs: Vec<u64> = vec![];
    if compat & 0x200 != 0 {
        for o in [0x24Cusize, 0x250] {
            let g = le32(p, o) as u64;
            if g != 0 && g < groups {
                gs.push(g);
            }
        }
    } else if ro & 0x1 != 0 {
        if groups > 1 {
            gs.push(1);
        }
        for base in [3u64, 5, 7] {
            let mut g = base;
            while g < groups {
                gs.push(g);
                g *= base;
            }
        }
    } else {
        gs.extend(1..groups);
    }
    gs.sort_unstable();
    gs.dedup();
    let mut v: Vec<Copy> = gs.into_iter().map(|g| Copy { group: g, off: (g * bpg + fdb) * bs }).collect();
    v.push(Copy { group: 0, off: 1024 });
    Ok(v)
}

fn read_at(f: &mut File, off: u64, n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    f.seek(SeekFrom::Start(off)).and_then(|_| f.read_exact(&mut b)).map_err(|e| format!("read @{}: {}", off, e))?;
    Ok(b)
}

fn flush_dev(f: &File) {
    unsafe { libc::ioctl(f.as_raw_fd(), 0x1261 as _) }; // BLKFLSBUF, best effort
}

/// Does this copy look like a superblock of the filesystem whose primary is `prim`?
fn plausible(_c: &Copy, b: &[u8], prim: &[u8]) -> Result<(), String> {
    if u16::from_le_bytes([b[0x38], b[0x39]]) != 0xEF53 {
        return Err("no magic".into());
    }
    if b[0x68..0x78] != prim[0x68..0x78] {
        return Err("UUID differs".into());
    }
    if !csum_ok(b) {
        return Err("bad checksum".into());
    }
    Ok(())
}

/// Set or clear the guard in every copy. `backups_only`: leave the primary alone
/// (rollback: restore_sb writes the primary). `hook(n)` runs after the n-th write
/// (test crash injection). Returns (copies written, copies skipped as not plausible).
pub fn apply(dev: &str, want: bool, backups_only: bool, log: &mut dyn FnMut(&str), hook: &mut dyn FnMut(usize)) -> Result<(usize, usize), String> {
    let mut f = OpenOptions::new().read(true).write(true).open(dev).map_err(|e| format!("{}: {}", dev, e))?;
    let prim = read_at(&mut f, 1024, 1024)?;
    let list = copies(&prim)?;
    let bs = 1024u64 << le32(&prim, 0x18);
    let _ = bs;
    let (mut done, mut skipped) = (0usize, 0usize);
    for c in &list {
        if backups_only && c.group == 0 {
            continue;
        }
        let mut b = read_at(&mut f, c.off, 1024)?;
        if let Err(why) = plausible(c, &b, &prim) {
            if c.group == 0 {
                return Err(format!("primary superblock: {}", why));
            }
            log(&format!("guard: group {} copy @{} skipped ({})", c.group, c.off, why));
            skipped += 1;
            continue;
        }
        let has = le32(&b, 0x60) & GUARD != 0;
        if has == want {
            continue;
        }
        let inc = if want { le32(&b, 0x60) | GUARD } else { le32(&b, 0x60) & !GUARD };
        b[0x60..0x64].copy_from_slice(&inc.to_le_bytes());
        if csum_on(&b) {
            let c32 = crc32c(!0, &b[..0x3FC]);
            b[0x3FC..0x400].copy_from_slice(&c32.to_le_bytes());
        }
        f.seek(SeekFrom::Start(c.off)).and_then(|_| f.write_all(&b)).map_err(|e| format!("write @{}: {}", c.off, e))?;
        f.sync_all().map_err(|e| e.to_string())?;
        flush_dev(&f);
        done += 1;
        hook(done);
    }
    drop(f);
    verify(dev, want, backups_only)?;
    Ok((done, skipped))
}

/// Re-read every plausible copy: guard state as wanted and checksum valid.
pub fn verify(dev: &str, want: bool, backups_only: bool) -> Result<usize, String> {
    let mut f = File::open(dev).map_err(|e| format!("{}: {}", dev, e))?;
    let prim = read_at(&mut f, 1024, 1024)?;
    let mut n = 0;
    for c in copies(&prim)? {
        if backups_only && c.group == 0 {
            continue;
        }
        let b = read_at(&mut f, c.off, 1024)?;
        if plausible(&c, &b, &prim).is_err() {
            continue; // never written by us, either
        }
        if (le32(&b, 0x60) & GUARD != 0) != want {
            return Err(format!("group {} superblock: guard bit is {} after the write", c.group, if want { "missing" } else { "still set" }));
        }
        n += 1;
    }
    Ok(n)
}

/// The first plausible backup superblock: (group, bytes). None if there is none.
pub fn first_backup(dev: &str) -> Result<Option<(u64, Vec<u8>)>, String> {
    let mut f = File::open(dev).map_err(|e| format!("{}: {}", dev, e))?;
    let prim = read_at(&mut f, 1024, 1024)?;
    for c in copies(&prim)? {
        if c.group == 0 {
            continue;
        }
        let b = read_at(&mut f, c.off, 1024)?;
        if plausible(&c, &b, &prim).is_ok() {
            return Ok(Some((c.group, b)));
        }
    }
    Ok(None)
}

/// The backup superblock of `group` as it is now (None if the group has no plausible copy).
pub fn backup_of(dev: &str, group: u64) -> Result<Option<Vec<u8>>, String> {
    let mut f = File::open(dev).map_err(|e| format!("{}: {}", dev, e))?;
    let prim = read_at(&mut f, 1024, 1024)?;
    for c in copies(&prim)? {
        if c.group == group {
            let b = read_at(&mut f, c.off, 1024)?;
            return Ok(if plausible(&c, &b, &prim).is_ok() { Some(b) } else { None });
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_known() {
        assert_eq!(crc32c(!0, b"123456789") ^ !0, 0xE306_9283);
    }
}
