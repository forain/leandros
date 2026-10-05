//! Block-device types shared by every backend and by both façades
//! (`blkdev.rs`, the runtime registry, and `blkdev_sdhci.rs`, the
//! compile-time SD-card path on rpi5/raspi4b).
//!
//! Always compiled, so a driver can implement [`BlockDevice`] regardless of
//! which façade the build selects as `drivers::blkdev`.

/// A whole disk as a driver presents it to the block registry.
///
/// Addressing is in the device's own logical sectors. Buffers passed to
/// `read_sectors`/`write_sectors` are always a non-zero multiple of
/// `sector_size()` bytes and may be arbitrarily long: the implementation
/// splits them into whatever transfer size its hardware supports.
///
/// **Locking contract:** methods take `&self` and may be called concurrently
/// from several threads; each device serializes its own requests (typically a
/// `spin::Mutex` around its queue state). The registry never holds a global
/// lock while calling into a device.
pub trait BlockDevice: Send + Sync {
    /// Logical sector size in bytes: 512 or 4096 (must divide 4096).
    fn sector_size(&self) -> u32;
    /// Capacity in logical sectors; 0 when the device cannot tell.
    fn capacity_sectors(&self) -> u64;
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkErr>;
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlkErr>;
    /// Commit any volatile write cache to stable media.
    fn flush(&self) -> Result<(), BlkErr>;
    /// True for hot-pluggable media (USB mass storage, SD readers).
    fn removable(&self) -> bool;
    /// Linux name class: `"vd"` for virtio-blk, `"sd"` for SCSI/USB mass
    /// storage. The registry appends a letter (`sda`, `sdb`, …).
    fn name_class(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkErr {
    /// The device reported an error or timed out.
    Io,
    /// Request runs past the end of the device or partition.
    OutOfRange,
    /// Buffer length is zero or not a multiple of the sector size.
    BadBuffer,
    /// No such registry index.
    NoDevice,
    /// The device's sector size is not 512..=4096 dividing 4096.
    Unsupported,
    /// The registry is full.
    Full,
}

/// Longest registry name (`sdz15` and `mmcblk0p15` fit).
pub const NAME_MAX: usize = 16;

/// A registry entry as the VFS block layer needs to see it (no I/O to
/// produce, so it is safe to call with VFS locks held).
#[derive(Clone, Copy, Debug)]
pub struct DiskDesc {
    pub name: [u8; NAME_MAX],
    pub nlen: u8,
    /// Registry index of the whole disk for a partition; `None` for a disk.
    pub parent: Option<usize>,
    /// 1-based partition number (0 for a whole disk).
    pub pno: u32,
    /// Byte offset within the parent disk (0 for a whole disk).
    pub start_bytes: u64,
    /// Size in bytes (0 when unknown).
    pub bytes: u64,
    pub sector_size: u32,
    pub removable: bool,
    /// `"vd"` / `"sd"` (inherited from the disk for a partition).
    pub class: &'static str,
    /// Per-class disk ordinal: vda = 0, vdb = 1, sda = 0 … (the parent's,
    /// for a partition). Drives Linux-style minor numbers in the VFS.
    pub ordinal: u16,
}

impl DiskDesc {
    pub fn name(&self) -> &[u8] { &self.name[..self.nlen as usize] }
}

/// Why `/dev/root` did not resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootErr {
    /// Nothing matches (yet) — the caller may retry after late enumeration.
    NotFound,
    /// More than one equally good candidate; refusing to guess.
    Ambiguous,
}

/// Identity of the disk the bootloader loaded the kernel from (Limine
/// `ExecutableFileRequest`): MBR disk signature or GPT disk GUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDisk {
    Mbr(u32),
    Gpt([u8; 16]),
}

pub const F2FS_MAGIC: u32 = 0xF2F5_2010;
/// Byte offset of the f2fs superblock magic within the volume's first block.
pub const F2FS_SB_OFFSET: usize = 1024;

/// True when `block0` (the first 4096 bytes of a volume) carries f2fs magic.
pub fn is_f2fs_block0(block0: &[u8]) -> bool {
    block0.len() >= F2FS_SB_OFFSET + 4
        && u32::from_le_bytes(block0[F2FS_SB_OFFSET..F2FS_SB_OFFSET + 4].try_into().unwrap())
            == F2FS_MAGIC
}

/// The value of the last `root=` token on a kernel command line.
pub fn parse_root_arg(cmdline: &str) -> Option<&str> {
    cmdline.split_ascii_whitespace()
        .filter_map(|t| t.strip_prefix("root="))
        .filter(|v| !v.is_empty())
        .last()
}
