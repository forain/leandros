//! Host tests for the f2fs server's checkpoint-flag module (included verbatim).
#[path = "../../f2fs/src/cpflags.rs"]
pub mod cpflags;

#[cfg(test)]
mod tests {
    use super::cpflags::*;

    #[test]
    fn constants_match_linux_f2fs_fs_h() {
        assert_eq!(CP_UMOUNT_FLAG, 0x1);
        assert_eq!(CP_FSCK_FLAG, 0x10);
        assert_eq!(CP_ERROR_FLAG, 0x8);
        assert_eq!(CP_CRC_RECOVERY_FLAG, 0x40);
        assert_eq!(CP_NOCRC_RECOVERY_FLAG, 0x200);
        assert_eq!(CP_LARGE_NAT_BITMAP_FLAG, 0x400);
        assert_eq!(CP_QUOTA_NEED_FSCK_FLAG, 0x800);
        assert_eq!(CP_DISABLED_FLAG, 0x1000);
    }

    #[test]
    fn nocrc_is_not_a_layout_problem_or_fsck_request() {
        assert!(!cp_large_nat_bitmap(CP_NOCRC_RECOVERY_FLAG));
        assert!(!cp_needs_fsck(CP_NOCRC_RECOVERY_FLAG | CP_CRC_RECOVERY_FLAG | CP_UMOUNT_FLAG));
    }

    #[test]
    fn large_nat_bitmap_is_layout_not_quota() {
        assert!(cp_large_nat_bitmap(CP_LARGE_NAT_BITMAP_FLAG));
        assert!(!cp_needs_fsck(CP_LARGE_NAT_BITMAP_FLAG));
    }

    #[test]
    fn quota_need_fsck_is_detected() {
        assert!(cp_needs_fsck(CP_QUOTA_NEED_FSCK_FLAG));
        assert!(!cp_large_nat_bitmap(CP_QUOTA_NEED_FSCK_FLAG));
        for f in [CP_FSCK_FLAG, CP_ERROR_FLAG, CP_DISABLED_FLAG] { assert!(cp_needs_fsck(f)); }
    }

    #[test]
    fn own_volume_flags_unchanged() {
        // LeandrOS writes only 0 or UMOUNT (and later 0x40): none ask for fsck or a bitmap layout.
        for f in [0, CP_UMOUNT_FLAG, CP_CRC_RECOVERY_FLAG, CP_CRC_RECOVERY_FLAG | CP_UMOUNT_FLAG] {
            assert!(!cp_needs_fsck(f) && !cp_large_nat_bitmap(f));
        }
    }

    #[test]
    fn chain_stamp_modes() {
        let (ver, crc) = (0x1_0000_0007u64, 0xdead_beefu32);
        assert_eq!(chain_want(ver, None, 0), Some(ver));
        assert_eq!(chain_want(ver, None, CP_CRC_RECOVERY_FLAG), None);
        assert_eq!(chain_want(ver, Some(crc), CP_CRC_RECOVERY_FLAG), Some(ver | (crc as u64) << 32));
        // NOCRC: no crc needed, full ver kept, low 32 bits compared.
        let f = CP_CRC_RECOVERY_FLAG | CP_NOCRC_RECOVERY_FLAG;
        assert_eq!(chain_want(ver, None, f), Some(ver));
    }

    #[test]
    fn chain_match_crc_vs_nocrc() {
        let want = 0xdead_beef_0000_0007u64;
        let crc = CP_CRC_RECOVERY_FLAG;
        assert!(chain_matches(want, want, crc));
        assert!(!chain_matches(want, 0x0000_0000_0000_0007, crc));
        let nocrc = crc | CP_NOCRC_RECOVERY_FLAG;
        assert!(chain_matches(0x1_0000_0007, 0xdead_beef_0000_0007, nocrc));
        assert!(!chain_matches(0x1_0000_0007, 0xdead_beef_0000_0008, nocrc));
        // without the CRC flag it is exact
        assert!(!chain_matches(0x1_0000_0007, 0x0000_0007, 0));
    }

    #[test]
    fn only_fsync_bit_marks_a_chain_node() {
        assert!(node_is_fsync_mark(0x2));
        assert!(node_is_fsync_mark(0x1 | 0x2 | 0x4 | (7 << 3)));
        assert!(!node_is_fsync_mark(0x4));
        assert!(!node_is_fsync_mark(0x1 | 0x4 | (3 << 3)));
        assert!(!node_is_fsync_mark(0));
    }

    #[test]
    fn large_nat_bitmap_detected_with_other_flags() {
        assert!(cp_large_nat_bitmap(CP_LARGE_NAT_BITMAP_FLAG | CP_UMOUNT_FLAG | CP_CRC_RECOVERY_FLAG));
    }
}
