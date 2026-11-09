//! FUSE kernel protocol 7.x wire constants and little-endian field access.

pub const IN_HDR: usize = 40;
pub const OUT_HDR: usize = 16;
pub const ATTR_SZ: usize = 88;
pub const ENTRY_SZ: usize = 40 + ATTR_SZ;
pub const KERNEL_MINOR: u32 = 31;

pub const LOOKUP: u32 = 1;
pub const FORGET: u32 = 2;
pub const GETATTR: u32 = 3;
pub const SETATTR: u32 = 4;
pub const READLINK: u32 = 5;
pub const SYMLINK: u32 = 6;
pub const MKNOD: u32 = 8;
pub const MKDIR: u32 = 9;
pub const UNLINK: u32 = 10;
pub const RMDIR: u32 = 11;
pub const RENAME: u32 = 12;
pub const LINK: u32 = 13;
pub const OPEN: u32 = 14;
pub const READ: u32 = 15;
pub const WRITE: u32 = 16;
pub const STATFS: u32 = 17;
pub const RELEASE: u32 = 18;
pub const FSYNC: u32 = 20;
pub const SETXATTR: u32 = 21;
pub const GETXATTR: u32 = 22;
pub const LISTXATTR: u32 = 23;
pub const REMOVEXATTR: u32 = 24;
pub const FLUSH: u32 = 25;
pub const INIT: u32 = 26;
pub const OPENDIR: u32 = 27;
pub const READDIR: u32 = 28;
pub const RELEASEDIR: u32 = 29;
pub const FSYNCDIR: u32 = 30;
pub const ACCESS: u32 = 34;
pub const CREATE: u32 = 35;
pub const INTERRUPT: u32 = 36;
pub const DESTROY: u32 = 38;
pub const FALLOCATE: u32 = 43;
pub const RENAME2: u32 = 45;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFLNK: u32 = 0o120000;

pub fn u64_at(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}
pub fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

/// `fuse_attr` accessors over an 88-byte slice.
#[derive(Clone, Copy)]
pub struct Attr(pub [u8; ATTR_SZ]);
impl Attr {
    pub fn from(b: &[u8]) -> Attr {
        let mut a = [0u8; ATTR_SZ];
        let n = b.len().min(ATTR_SZ);
        a[..n].copy_from_slice(&b[..n]);
        Attr(a)
    }
    pub fn ino(&self) -> u64 { u64_at(&self.0, 0) }
    pub fn size(&self) -> u64 { u64_at(&self.0, 8) }
    pub fn blocks(&self) -> u64 { u64_at(&self.0, 16) }
    pub fn atime(&self) -> (i64, i64) { (u64_at(&self.0, 24) as i64, u32_at(&self.0, 48) as i64) }
    pub fn mtime(&self) -> (i64, i64) { (u64_at(&self.0, 32) as i64, u32_at(&self.0, 52) as i64) }
    pub fn ctime(&self) -> (i64, i64) { (u64_at(&self.0, 40) as i64, u32_at(&self.0, 56) as i64) }
    pub fn mode(&self) -> u32 { u32_at(&self.0, 60) }
    pub fn nlink(&self) -> u32 { u32_at(&self.0, 64) }
    pub fn uid(&self) -> u32 { u32_at(&self.0, 68) }
    pub fn gid(&self) -> u32 { u32_at(&self.0, 72) }
    pub fn rdev(&self) -> u32 { u32_at(&self.0, 76) }
    pub fn is_dir(&self) -> bool { self.mode() & S_IFMT == S_IFDIR }
    pub fn is_lnk(&self) -> bool { self.mode() & S_IFMT == S_IFLNK }
}
