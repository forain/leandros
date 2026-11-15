//! Checkpoint `ckpt_flags` bits and the pure decisions made from them.
//! Values are Linux's (`include/f2fs_fs.h`, `CP_*_FLAG`). Kept free of any
//! device access so the host tests in `servers/f2fs-tests` include it verbatim.

pub const CP_UMOUNT_FLAG: u32 = 0x0000_0001;
pub const CP_ORPHAN_PRESENT_FLAG: u32 = 0x0000_0002;
pub const CP_COMPACT_SUM_FLAG: u32 = 0x0000_0004;
pub const CP_ERROR_FLAG: u32 = 0x0000_0008;
pub const CP_FSCK_FLAG: u32 = 0x0000_0010;
pub const CP_FASTBOOT_FLAG: u32 = 0x0000_0020;
pub const CP_CRC_RECOVERY_FLAG: u32 = 0x0000_0040;
pub const CP_NAT_BITS_FLAG: u32 = 0x0000_0080;
pub const CP_TRIMMED_FLAG: u32 = 0x0000_0100;
/// fsck.f2fs sets it after a fix: node footers carry only the low 32 bits of cp_ver.
pub const CP_NOCRC_RECOVERY_FLAG: u32 = 0x0000_0200;
/// NAT bitmap larger than the commit block's private area.
pub const CP_LARGE_NAT_BITMAP_FLAG: u32 = 0x0000_0400;
pub const CP_QUOTA_NEED_FSCK_FLAG: u32 = 0x0000_0800;
pub const CP_DISABLED_FLAG: u32 = 0x0000_1000;

/// Flags that make a checkpoint untrustworthy for a read-write mount.
pub const CP_NEEDS_FSCK_MASK: u32 =
    CP_FSCK_FLAG | CP_ERROR_FLAG | CP_DISABLED_FLAG | CP_QUOTA_NEED_FSCK_FLAG;

pub fn cp_needs_fsck(flags: u32) -> bool { flags & CP_NEEDS_FSCK_MASK != 0 }

/// The SIT/NAT bitmaps are not in the layout `linux_import` reads.
pub fn cp_large_nat_bitmap(flags: u32) -> bool { flags & CP_LARGE_NAT_BITMAP_FLAG != 0 }

/// Does the checkpoint stamp its fsync chain with the commit-block crc too?
pub fn cp_crc_recovery(flags: u32) -> bool { flags & CP_CRC_RECOVERY_FLAG != 0 }

/// The `cp_ver` a recoverable node footer must carry (Linux `f2fs_recover_fsync_data`).
/// `crc` is the checkpoint's own checksum field; `None` when it cannot be read.
/// Returns `None` when the flags demand a crc that is not available.
pub fn chain_want(ver: u64, crc: Option<u32>, flags: u32) -> Option<u64> {
    if !cp_crc_recovery(flags) { return Some(ver); }
    if flags & CP_NOCRC_RECOVERY_FLAG != 0 { return Some(ver); }
    crc.map(|c| ver | ((c as u64) << 32))
}

/// Is a node footer's `cp_ver` a match for the checkpoint? With NOCRC set only
/// the low 32 bits count (Linux `is_recoverable_dnode`).
pub fn chain_matches(want: u64, node_ver: u64, flags: u32) -> bool {
    if cp_crc_recovery(flags) && flags & CP_NOCRC_RECOVERY_FLAG != 0 {
        (want as u32) == (node_ver as u32)
    } else {
        want == node_ver
    }
}
