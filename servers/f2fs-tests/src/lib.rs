//! Host tests for the f2fs server's checkpoint-flag module (included verbatim).
#[path = "../../f2fs/src/cpflags.rs"]
pub mod cpflags;
#[path = "../../f2fs/src/lxchain.rs"]
pub mod lxchain;

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

    mod lx {
        use super::super::lxchain::*;

        #[test]
        fn footer_flag_table() {
            assert_eq!(footer_flag(0, true, true), 0x7);   // new inode: COLD|FSYNC|DENT
            assert_eq!(footer_flag(0, true, false), 0x3);  // old inode
            assert_eq!(footer_flag(5, false, true), (5 << 3) | 0x3); // dnode never DENT
            assert_eq!(footer_flag(0x1FFF_FFFF >> 3, false, false) & 7, 3); // xattr-like
        }

        #[test]
        fn indirect_predicate() {
            assert!(lx_ofs_indirect(OFS_IND1) && lx_ofs_indirect(OFS_IND2) && lx_ofs_indirect(OFS_DIND));
            assert!(lx_ofs_indirect(OFS_DIND + 1)); // first node under DIND is an indirect
            assert!(!lx_ofs_indirect(OFS_DIND + 2)); // a dnode under it
            assert!(!lx_ofs_indirect(0) && !lx_ofs_indirect(1) && !lx_ofs_indirect(2));
            assert!(!lx_ofs_indirect(0x1FFF_FFFF)); // xattr node
            assert!(lx_ofs_indirect(OFS_DIND + 1 + (LX_N + 1)));
        }

        #[test]
        fn stamp_layout() {
            let mut b = [0u8; BLOCK];
            b[FT_NID..FT_NID + 4].copy_from_slice(&7u32.to_le_bytes());
            b[FT_INO..FT_INO + 4].copy_from_slice(&9u32.to_le_bytes());
            stamp_copy(&mut b, 0x1122_3344_5566_7788, 0xAABB_CCDD, 0x17);
            assert_eq!(&b[4080..4084], &0x17u32.to_le_bytes());
            assert_eq!(&b[4084..4092], &0x1122_3344_5566_7788u64.to_le_bytes());
            assert_eq!(&b[4092..4096], &0xAABB_CCDDu32.to_le_bytes());
            assert_eq!(&b[4072..4076], &7u32.to_le_bytes());
            assert_eq!(&b[4076..4080], &9u32.to_le_bytes());
        }

        #[test]
        fn head_crc_big_differs_and_validates() {
            let mut cp = [0u8; BLOCK];
            for (i, x) in cp.iter_mut().enumerate().take(CP_CRC_OFF) { *x = (i * 7 + 1) as u8; }
            seal(&mut cp);
            assert_eq!(head_crc(&cp, false), u32::from_le_bytes(cp[CP_CRC_OFF..].try_into().unwrap()));
            assert_ne!(head_crc(&cp, true), head_crc(&cp, false));
            let h = head_block(&cp, true);
            assert!(h[CP_OWN_VER..CP_CRC_OFF].iter().all(|&x| x == 0));
            assert_eq!(linux_crc(&h[..CP_CRC_OFF]), u32::from_le_bytes(h[CP_CRC_OFF..].try_into().unwrap()));
            // zero private area: big and !big agree
            let mut z = cp; z[CP_OWN_VER..CP_CRC_OFF].fill(0); seal(&mut z);
            assert_eq!(head_crc(&z, true), head_crc(&z, false));
        }
    }
}
