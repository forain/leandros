#!/usr/bin/env python3
"""Create a minimal valid F2FS image for LeandrOS testing.

Produces a 64 MB F2FS volume with an empty root directory.
Matches the on-disk layout expected by servers/f2fs/src/lib.rs.
"""
import struct, sys, os

BLOCK_SIZE     = 4096
BLOCKS_PER_SEG = 512       # 2 MB segments (log_blocks_per_seg = 9)
TOTAL_BLOCKS   = 16384     # 64 MB / 4 KB
TOTAL_SEGS     = TOTAL_BLOCKS // BLOCKS_PER_SEG   # 32
F2FS_MAGIC     = 0xF2F52010
CP_UMOUNT_FLAG = 0x00000008

# Segment layout ---------------------------------------------------------
# Segment 0 (blk 0–511)  : reserved / superblock
SEGMENT0_BLKADDR = BLOCKS_PER_SEG      # 512
SEG_CNT_CKPT = 2   # 2 segs → 2 packs (1 seg each)
SEG_CNT_SIT  = 1
SEG_CNT_NAT  = 2
SEG_CNT_SSA  = 1

CP_BLKADDR   = SEGMENT0_BLKADDR
SIT_BLKADDR  = CP_BLKADDR  + SEG_CNT_CKPT * BLOCKS_PER_SEG
NAT_BLKADDR  = SIT_BLKADDR + SEG_CNT_SIT  * BLOCKS_PER_SEG
SSA_BLKADDR  = NAT_BLKADDR + SEG_CNT_NAT  * BLOCKS_PER_SEG
MAIN_BLKADDR = SSA_BLKADDR + SEG_CNT_SSA  * BLOCKS_PER_SEG

# 1 reserved + 2 ckpt + 1 sit + 2 nat + 1 ssa = 8 meta segments; rest = main
META_SEGS  = 1 + SEG_CNT_CKPT + SEG_CNT_SIT + SEG_CNT_NAT + SEG_CNT_SSA
MAIN_SEGS  = TOTAL_SEGS - META_SEGS

NODE_INO = 1
META_INO = 2
ROOT_INO = 3

NAT_ENTRY_SIZE = 9    # version(1) + ino(4) + blkaddr(4)
SIT_ENTRY_SIZE = 74   # vblocks(2) + valid_map(64) + mtime(8)
SIT_PER_BLK    = BLOCK_SIZE // SIT_ENTRY_SIZE  # 55
MAX_ACTIVE_LOGS = 8   # MAX_ACTIVE_NODE_LOGS / MAX_ACTIVE_DATA_LOGS
NODE_FOOTER_OFF = 4076

def w8 (b, o, v): b[o] = v & 0xFF
def w16(b, o, v): struct.pack_into('<H', b, o, v & 0xFFFF)
def w32(b, o, v): struct.pack_into('<I', b, o, v & 0xFFFFFFFF)
def w64(b, o, v): struct.pack_into('<Q', b, o, v & 0xFFFFFFFFFFFFFFFF)

image = bytearray(TOTAL_BLOCKS * BLOCK_SIZE)

# ── Superblock (block 0, offset 1024) ──────────────────────────────────────
def write_superblock(image, base):
    s = base + 1024
    w32(image, s +  0, F2FS_MAGIC)
    w16(image, s +  4, 1)     # major_ver
    w16(image, s +  6, 12)    # minor_ver
    w32(image, s +  8, 9)     # log_sectorsize  (512-byte sectors)
    w32(image, s + 12, 3)     # log_sectors_per_block (8 sectors × 512 = 4096)
    w32(image, s + 16, 12)    # log_blocksize   (4096)
    w32(image, s + 20, 9)     # log_blocks_per_seg (512)
    w32(image, s + 24, 1)     # segs_per_sec
    w32(image, s + 28, 1)     # secs_per_zone
    w32(image, s + 32, 0)     # checksum_offset
    w64(image, s + 36, TOTAL_BLOCKS)
    w32(image, s + 44, MAIN_SEGS)
    w32(image, s + 48, TOTAL_SEGS)
    w32(image, s + 52, SEG_CNT_CKPT)
    w32(image, s + 56, SEG_CNT_SIT)
    w32(image, s + 60, SEG_CNT_NAT)
    w32(image, s + 64, SEG_CNT_SSA)
    w32(image, s + 68, MAIN_SEGS)
    w32(image, s + 72, SEGMENT0_BLKADDR)
    w32(image, s + 76, CP_BLKADDR)
    w32(image, s + 80, SIT_BLKADDR)
    w32(image, s + 84, NAT_BLKADDR)
    w32(image, s + 88, SSA_BLKADDR)
    w32(image, s + 92, MAIN_BLKADDR)
    w32(image, s + 96, ROOT_INO)
    w32(image, s +100, NODE_INO)
    w32(image, s +104, META_INO)

write_superblock(image, 0)
write_superblock(image, BLOCK_SIZE)   # duplicate at block 1

# ── Checkpoint pack 0 (version 1, active) ──────────────────────────────────
def write_checkpoint(image, blkaddr, ver):
    o = blkaddr * BLOCK_SIZE
    w64(image, o +  0, ver)                # checkpoint_ver
    w64(image, o +  8, 0)                  # user_block_count
    w64(image, o + 16, 1)                  # valid_block_count  (root inode)
    w32(image, o + 24, 0)                  # rsvd_segment_count
    w32(image, o + 28, 1)                  # overprov_segment_count
    w32(image, o + 32, MAIN_SEGS - 1)     # free_segment_count
    # cur_node_segno[8]
    for i in range(MAX_ACTIVE_LOGS):
        w32(image, o + 36 + i * 4, 0xFFFFFFFF if i > 0 else 0)
    # cur_node_blkoff[8]
    for i in range(MAX_ACTIVE_LOGS):
        w16(image, o + 68 + i * 2, 1 if i == 0 else 0)
    # cur_data_segno[8]
    for i in range(MAX_ACTIVE_LOGS):
        w32(image, o + 84 + i * 4, 0xFFFFFFFF if i > 0 else 1)
    # cur_data_blkoff[8]
    for i in range(MAX_ACTIVE_LOGS):
        w16(image, o + 116 + i * 2, 0)
    w32(image, o + 132, CP_UMOUNT_FLAG)    # ckpt_flags
    w32(image, o + 136, 5)                 # cp_pack_total_block_count
    w32(image, o + 140, 1)                 # cp_pack_start_sum
    w32(image, o + 144, 1)                 # valid_node_count  (root inode)
    w32(image, o + 148, 1)                 # valid_inode_count
    w32(image, o + 152, ROOT_INO + 10)    # next_free_nid

write_checkpoint(image, CP_BLKADDR, 1)                    # pack 0 — newer
write_checkpoint(image, CP_BLKADDR + BLOCKS_PER_SEG, 0)   # pack 1 — older

# ── NAT: entries for node_ino=1, meta_ino=2, root_ino=3 ───────────────────
def write_nat_entry(image, ino, blk_addr):
    nat_blk = NAT_BLKADDR + ino // (BLOCK_SIZE // NAT_ENTRY_SIZE)
    idx     = ino % (BLOCK_SIZE // NAT_ENTRY_SIZE)
    o = nat_blk * BLOCK_SIZE + idx * NAT_ENTRY_SIZE
    w8 (image, o + 0, 1)          # version
    w32(image, o + 1, ino)        # ino
    w32(image, o + 5, blk_addr)   # blk_addr

write_nat_entry(image, NODE_INO, MAIN_BLKADDR + 1)
write_nat_entry(image, META_INO, MAIN_BLKADDR + 2)
write_nat_entry(image, ROOT_INO, MAIN_BLKADDR + 0)  # root inode at first main block

# ── Root inode (block MAIN_BLKADDR) ────────────────────────────────────────
o = MAIN_BLKADDR * BLOCK_SIZE
w16(image, o +  0, 0o040755)   # i_mode = directory, rwxr-xr-x
w8 (image, o +  2, 0)          # i_advise
w8 (image, o +  3, 0)          # i_inline = 0 (no inline data/dentry)
w32(image, o + 12, 2)          # i_links = 2 (. and ..)
w64(image, o + 16, 0)          # i_size = 0
# Node footer (at NODE_FOOTER_OFF = 4076)
w32(image, o + NODE_FOOTER_OFF + 0, ROOT_INO)  # nid
w32(image, o + NODE_FOOTER_OFF + 4, ROOT_INO)  # ino
w32(image, o + NODE_FOOTER_OFF + 8, 0)          # index
w32(image, o + NODE_FOOTER_OFF + 12, 0)         # flag
w32(image, o + NODE_FOOTER_OFF + 16, 1)         # cp_ver

# ── SIT: mark used segments ────────────────────────────────────────────────
MAIN_START_SEG = MAIN_BLKADDR // BLOCKS_PER_SEG

def sit_entry_offset(seg):
    blk_idx   = seg // SIT_PER_BLK
    entry_idx = seg % SIT_PER_BLK
    return (SIT_BLKADDR + blk_idx) * BLOCK_SIZE + entry_idx * SIT_ENTRY_SIZE

# All meta segments fully used
for seg in range(MAIN_START_SEG):
    o = sit_entry_offset(seg)
    w16(image, o, BLOCKS_PER_SEG)
    for b in range(64): image[o + 2 + b] = 0xFF

# First main segment: root inode uses 1 block
o = sit_entry_offset(MAIN_START_SEG)
w16(image, o, 1)
image[o + 2] = 0x80  # bit 7 set = block 0 used

# ── Write image ────────────────────────────────────────────────────────────
out = sys.argv[1] if len(sys.argv) > 1 else 'f2fs-data.img'
with open(out, 'wb') as f:
    f.write(image)

sz = len(image) // (1024 * 1024)
print(f"[mkfs-f2fs-minimal] Created {sz}MB F2FS image: {out}")
print(f"  MAIN_BLKADDR={MAIN_BLKADDR} ROOT_INO={ROOT_INO} CP_BLKADDR={CP_BLKADDR}")
