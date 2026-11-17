//! Pure helpers of the Linux warm-node fsync chain (lane f2fsnlog1, knob
//! `f2fs.lxchain`). Free of device access so the host tests in
//! `servers/f2fs-tests` include it verbatim. See design-f2fsnodelog-p1.md.

pub const BLOCK: usize = 4096;
/// Footer offsets inside a node block (struct node_footer, 24 bytes at 4072).
pub const FT_NID: usize = 4072;
pub const FT_INO: usize = 4076;
pub const FT_FLAG: usize = 4080;
pub const FT_CP_VER: usize = 4084;
pub const FT_NEXT: usize = 4092;
/// Commit block: private area start (SIT/NAT version bitmaps in Linux use it
/// on big_cp volumes), checksum field, CRC.
pub const CP_OWN_VER: usize = 2048;
pub const CP_CRC_OFF: usize = 4092;
pub const CP_CHKSUM_FIELD: usize = 164;

pub const NODE_FLAG_COLD: u32 = 0x1;
pub const NODE_FLAG_FSYNC: u32 = 0x2;
pub const NODE_FLAG_DENT: u32 = 0x4;

/// Mirror of lib.rs `LX_N` and the indirect node offsets (asserted equal in lib.rs).
pub const LX_N: u32 = 1018;
pub const OFS_IND1: u32 = 3;
pub const OFS_IND2: u32 = 4 + LX_N;
pub const OFS_DIND: u32 = 5 + 2 * LX_N;

/// Is the node at footer offset `ofs` an indirect node (IND1/IND2/DIND or a
/// node under DIND)? Linux does not recover those through the fsync chain.
pub fn lx_ofs_indirect(ofs: u32) -> bool {
    ofs == OFS_IND1 || ofs == OFS_IND2 || ofs == OFS_DIND
        || (ofs > OFS_DIND && (ofs - OFS_DIND - 1) % (LX_N + 1) == 0)
}

/// Footer flag word: `ofs << 3 | COLD | FSYNC | (DENT on an inode)`.
pub fn footer_flag(ofs: u32, is_inode: bool, dent: bool) -> u32 {
    (ofs << 3) | NODE_FLAG_COLD | NODE_FLAG_FSYNC | if dent && is_inode { NODE_FLAG_DENT } else { 0 }
}

fn w32(b: &mut [u8], off: usize, v: u32) { b[off..off + 4].copy_from_slice(&v.to_le_bytes()); }
fn r32(b: &[u8], off: usize) -> u32 { u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) }

/// Stamp a chain copy: flag, the 64-bit cp_ver stamp and the next block
/// address. nid and ino stay as they are.
pub fn stamp_copy(buf: &mut [u8; BLOCK], stamp: u64, next_blk: u32, flag: u32) {
    w32(buf, FT_FLAG, flag);
    buf[FT_CP_VER..FT_CP_VER + 8].copy_from_slice(&stamp.to_le_bytes());
    w32(buf, FT_NEXT, next_blk);
}

/// Linux f2fs_crc32 (crc32_le seeded with the superblock magic).
pub fn linux_crc(data: &[u8]) -> u32 {
    let mut c: u32 = 0xF2F5_2010;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 { c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 }; }
    }
    c
}

/// Seal `b`: checksum_offset field and Linux CRC.
pub fn seal(b: &mut [u8; BLOCK]) {
    w32(b, CP_CHKSUM_FIELD, CP_CRC_OFF as u32);
    let c = linux_crc(&b[..CP_CRC_OFF]);
    w32(b, CP_CRC_OFF, c);
}

/// The head commit block as written at `cp_blkno`: on a big_cp volume Linux
/// owns the private area, so it is zeroed and the block resealed.
pub fn head_block(cp_buf: &[u8; BLOCK], big: bool) -> [u8; BLOCK] {
    let mut real = *cp_buf;
    if big {
        real[CP_OWN_VER..CP_CRC_OFF].fill(0);
        seal(&mut real);
    }
    real
}

/// The CRC field of the head commit block (what Linux's cur_cp_crc reads).
pub fn head_crc(cp_buf: &[u8; BLOCK], big: bool) -> u32 { r32(&head_block(cp_buf, big), CP_CRC_OFF) }
