#!/usr/bin/env python3
"""Create a pre-populated F2FS image for LeandrOS with all userland binaries.

Matches the on-disk layout expected by servers/f2fs/src/lib.rs.
"""
import struct
import sys
import os

BLOCK_SIZE     = 4096
BLOCKS_PER_SEG = 512       # 2 MB segments (log_blocks_per_seg = 9)
F2FS_MAGIC     = 0xF2F52010
CP_UMOUNT_FLAG = 0x00000008

# Segment layout
SEGMENT0_BLKADDR = BLOCKS_PER_SEG      # 512
SEG_CNT_CKPT = 2
SEG_CNT_SIT  = 1
SEG_CNT_NAT  = 2
SEG_CNT_SSA  = 1

CP_BLKADDR   = SEGMENT0_BLKADDR
SIT_BLKADDR  = CP_BLKADDR  + SEG_CNT_CKPT * BLOCKS_PER_SEG
NAT_BLKADDR  = SIT_BLKADDR + SEG_CNT_SIT  * BLOCKS_PER_SEG
SSA_BLKADDR  = NAT_BLKADDR + SEG_CNT_NAT  * BLOCKS_PER_SEG
MAIN_BLKADDR = SSA_BLKADDR + SEG_CNT_SSA  * BLOCKS_PER_SEG

META_SEGS  = 1 + SEG_CNT_CKPT + SEG_CNT_SIT + SEG_CNT_NAT + SEG_CNT_SSA

NODE_INO = 1
META_INO = 2
ROOT_INO = 3

NAT_ENTRY_SIZE = 9
SIT_ENTRY_SIZE = 74
SIT_PER_BLK    = BLOCK_SIZE // SIT_ENTRY_SIZE
MAX_ACTIVE_LOGS = 8
NODE_FOOTER_OFF = 4076

# Directory constants
NR_DENTRY_IN_BLK   = 214
DENTRY_BITMAP_SIZE = 27
DENTRY_RESERVED    = 3
DENTRY_ENTRIES_OFF = DENTRY_BITMAP_SIZE + DENTRY_RESERVED # 30
DENTRY_SLOT_LEN    = 8
DENTRY_NAMES_OFF   = DENTRY_ENTRIES_OFF + NR_DENTRY_IN_BLK * 11 # 2384
DENTRY_ENTRY_SIZE  = 11

DT_DIR = 4
DT_REG = 8

def w8 (b, o, v): b[o] = v & 0xFF
def w16(b, o, v): struct.pack_into('<H', b, o, v & 0xFFFF)
def w32(b, o, v): struct.pack_into('<I', b, o, v & 0xFFFFFFFF)
def w64(b, o, v): struct.pack_into('<Q', b, o, v & 0xFFFFFFFFFFFFFFFF)

def write_superblock(image, base, total_blocks, total_segs, main_segs):
    s = base + 1024
    w32(image, s +  0, F2FS_MAGIC)
    w16(image, s +  4, 1)     # major_ver
    w16(image, s +  6, 12)    # minor_ver
    w32(image, s +  8, 9)     # log_sectorsize
    w32(image, s + 12, 3)     # log_sectors_per_block
    w32(image, s + 16, 12)    # log_blocksize
    w32(image, s + 20, 9)     # log_blocks_per_seg
    w32(image, s + 24, 1)     # segs_per_sec
    w32(image, s + 28, 1)     # secs_per_zone
    w32(image, s + 32, 0)     # checksum_offset
    w64(image, s + 36, total_blocks)
    w32(image, s + 44, main_segs)
    w32(image, s + 48, total_segs)
    w32(image, s + 52, SEG_CNT_CKPT)
    w32(image, s + 56, SEG_CNT_SIT)
    w32(image, s + 60, SEG_CNT_NAT)
    w32(image, s + 64, SEG_CNT_SSA)
    w32(image, s + 68, main_segs)
    w32(image, s + 72, SEGMENT0_BLKADDR)
    w32(image, s + 76, CP_BLKADDR)
    w32(image, s + 80, SIT_BLKADDR)
    w32(image, s + 84, NAT_BLKADDR)
    w32(image, s + 88, SSA_BLKADDR)
    w32(image, s + 92, MAIN_BLKADDR)
    w32(image, s + 96, ROOT_INO)
    w32(image, s +100, NODE_INO)
    w32(image, s +104, META_INO)

def build_dentry_block(entries):
    block = bytearray(4096)
    bitmap = bytearray(27)
    current_slot = 0
    for name, ino, ftype in entries:
        name_len = len(name)
        slots_used = (name_len + DENTRY_SLOT_LEN - 1) // DENTRY_SLOT_LEN
        if current_slot + slots_used > NR_DENTRY_IN_BLK:
            raise ValueError("Too many directory entries for one block!")
        for i in range(slots_used):
            slot = current_slot + i
            byte = slot // 8
            bit = slot % 8
            bitmap[byte] |= (1 << bit)
        e_off = DENTRY_ENTRIES_OFF + current_slot * DENTRY_ENTRY_SIZE
        w32(block, e_off + 0, 0)
        w32(block, e_off + 4, ino)
        w16(block, e_off + 8, name_len)
        block[e_off + 10] = ftype
        n_off = DENTRY_NAMES_OFF + current_slot * DENTRY_SLOT_LEN
        block[n_off : n_off + name_len] = name
        current_slot += slots_used
    block[0:27] = bitmap
    return block

def build_inode_block(mode, links, size, block_addrs, nid):
    block = bytearray(4096)
    w16(block, 0, mode)
    w8 (block, 2, 0)
    w8 (block, 3, 0)
    w32(block, 12, links)
    w64(block, 16, size)
    for idx, addr in enumerate(block_addrs):
        w32(block, 364 + idx * 4, addr)
    w32(block, NODE_FOOTER_OFF + 0, nid)
    w32(block, NODE_FOOTER_OFF + 4, nid)
    w32(block, NODE_FOOTER_OFF + 8, 0)
    w32(block, NODE_FOOTER_OFF + 12, 0)
    w32(block, NODE_FOOTER_OFF + 16, 1)
    return block

def write_checkpoint(image, blkaddr, ver, valid_blocks, valid_nodes, free_segs, next_nid, main_segs,
                      cur_node_segno, cur_node_blkoff, cur_data_segno, cur_data_blkoff):
    o = blkaddr * BLOCK_SIZE
    w64(image, o +  0, ver)
    w64(image, o +  8, 0)
    w64(image, o + 16, valid_blocks)
    w32(image, o + 24, 0)
    w32(image, o + 28, 1)
    w32(image, o + 32, free_segs)
    for i in range(MAX_ACTIVE_LOGS):
        w32(image, o + 36 + i * 4, 0xFFFFFFFF if i > 0 else cur_node_segno)
    for i in range(MAX_ACTIVE_LOGS):
        w16(image, o + 68 + i * 2, cur_node_blkoff if i == 0 else 0)
    for i in range(MAX_ACTIVE_LOGS):
        w32(image, o + 84 + i * 4, 0xFFFFFFFF if i > 0 else cur_data_segno)
    for i in range(MAX_ACTIVE_LOGS):
        w16(image, o + 116 + i * 2, cur_data_blkoff if i == 0 else 0)
    w32(image, o + 132, CP_UMOUNT_FLAG)
    w32(image, o + 136, 5)
    w32(image, o + 140, 1)
    w32(image, o + 144, valid_nodes)
    w32(image, o + 148, valid_nodes)
    w32(image, o + 152, next_nid)

def write_nat_entry(image, ino, blk_addr):
    nat_blk = NAT_BLKADDR + ino // (BLOCK_SIZE // NAT_ENTRY_SIZE)
    idx     = ino % (BLOCK_SIZE // NAT_ENTRY_SIZE)
    o = nat_blk * BLOCK_SIZE + idx * NAT_ENTRY_SIZE
    w8 (image, o + 0, 1)
    w32(image, o + 1, ino)
    w32(image, o + 5, blk_addr)

def main():
    if len(sys.argv) < 3:
        print("Usage: mkfs-f2fs-populated.py <output_img> <arch>")
        sys.exit(1)
        
    out_file = sys.argv[1]
    arch = sys.argv[2]
    
    # 1. Gather files to place in F2FS
    target_arch = "aarch64-unknown-none" if arch == "aarch64" else "x86_64-unknown-none"
    userland_dir = f"userland/target/{target_arch}/release"
    
    bin_files = []
    bins = [
        "shell", "hello", "aplay", "memtest", "vfstest", "f2fstest", "tput",
        "pthreadtest", "timertest", "sigtest", "polltest", "forktest", "racetest",
        "mount", "umount", "fstab", "lsblk", "lspci", "lsusb", "ping",
    ]
    for b in bins:
        p = os.path.join(userland_dir, b)
        if os.path.exists(p):
            bin_files.append((b, p, 0o100755))
            
    p = f"../doomgeneric/doom-{arch}"
    if os.path.exists(p):
        bin_files.append(("doom", p, 0o100755))
    p = "../doomgeneric/doom1.wad"
    if os.path.exists(p):
        bin_files.append(("doom1.wad", p, 0o100644))
        
    bottom_target = "aarch64-unknown-linux-musl" if arch == "aarch64" else "x86_64-unknown-linux-musl"
    p = f"../bottom-leandros/target/{bottom_target}/release/btm"
    if os.path.exists(p):
        bin_files.append(("btm", p, 0o100755))
        bin_files.append(("bottom", p, 0o100755))

    p = f"../mame/mame-{arch}"
    if os.path.exists(p):
        bin_files.append(("mame", p, 0o100755))
        
    lib_files = []
    relibc_target = "aarch64-unknown-leandros" if arch == "aarch64" else "x86_64-unknown-leandros"
    p = f"../relibc/target/{relibc_target}/release/librelibc.a"
    if os.path.exists(p):
        lib_files.append(("libc.a", p, 0o100644))
        
    root_files = []
    p = "userland/aplay/car-horn.wav"
    if os.path.exists(p):
        root_files.append(("car-horn.wav", p, 0o100644))

    # MAME ROM sets as zips in / — run with `mame -rompath / <game>`.
    # A zip sidesteps this script's flat directory model (no nested
    # /roms/<game>/ dirs needed); MAME's util/unzip.cpp reads it directly.
    p = "../mame/roms/captcomm.zip"
    if os.path.exists(p):
        root_files.append(("captcomm.zip", p, 0o100644))

    # /etc/fstab — vdb is the root F2FS volume userland/init already mounts
    # via a hardcoded bootstrap call before pivot_root (chicken-and-egg: fstab
    # itself lives on that filesystem); vdc is the second data disk QEMU has
    # always attached but nothing mounted until init started consulting this
    # file for secondary mounts.
    etc_files = [("fstab", (
        b"# <device>   <mountpoint>  <fstype>  <options>  <dump>  <pass>\n"
        b"/dev/vdb     /             f2fs      rw         0       1\n"
        b"/dev/vdc     /data         f2fs      rw         0       2\n"
    ), 0o100644)]
        
    # 2. Dynamically calculate required blocks and image size
    # Each meta segment takes 512 blocks. We have 8 meta segments (4096 blocks).
    # Plus safety margin and blocks for directories.
    def content_size(path):
        return len(path) if isinstance(path, (bytes, bytearray)) else os.path.getsize(path)

    required_blocks = 4096 + 100
    for name, path, mode in bin_files + lib_files + root_files + etc_files:
        size = content_size(path)
        k = (size + BLOCK_SIZE - 1) // BLOCK_SIZE
        # Inode block + data blocks + potential direct nodes.
        # ADDRS_PER_DNODE is 1019 in this simplified F2FS: the node footer
        # sits at byte 4076 (5xu32 = 20 bytes), leaving 4076/4 = 1019 slots.
        # This MUST match servers/f2fs/src/lib.rs (NODE_FOOTER_OFF / 4);
        # a 1018 here shifted every block past ~7.9 MB when read back.
        required_blocks += k + 1 + (k + 1018) // 1019
    # Reserve two full segments beyond the statically-populated content for the
    # runtime node/data allocator curseg pointers (see below) so first writes
    # never land on blocks already occupied by pre-populated files/directories.
    required_blocks += 2 * BLOCKS_PER_SEG
        
    # Align to segments (512 blocks) and set a minimum size of 64MB (16384 blocks).
    # Double the segment count beyond what's needed for the pre-populated content
    # so the image ships with roughly 50% free space — tests (f2fstest et al.)
    # write new files at runtime and would otherwise exhaust a snugly-sized image.
    segs = (required_blocks + 511) // 512
    total_segs = max(32, segs * 2)
    total_blocks = total_segs * BLOCKS_PER_SEG
    main_segs = total_segs - META_SEGS
    
    print(f"Sizing F2FS image dynamically to {total_blocks * BLOCK_SIZE // (1024 * 1024)} MB ({total_blocks} blocks, {total_segs} segments, {main_segs} main segments)...")
    
    image = bytearray(total_blocks * BLOCK_SIZE)
    write_superblock(image, 0, total_blocks, total_segs, main_segs)
    write_superblock(image, BLOCK_SIZE, total_blocks, total_segs, main_segs)
    
    # 3. Define statically allocated directory inodes and data blocks
    dir_nodes = {
        3: ("/"),
        4: ("/bin"),
        5: ("/old_root"),
        6: ("/dev"),
        7: ("/proc"),
        8: ("/tmp"),
        9: ("/etc"),
        10: ("/mnt"),
        11: ("/lib"),
    }
    
    inode_blocks = {}
    data_blocks = {}
    
    next_blk = MAIN_BLKADDR
    for ino in sorted(dir_nodes.keys()):
        inode_blocks[ino] = next_blk
        next_blk += 1
        
    for ino in sorted(dir_nodes.keys()):
        data_blocks[ino] = next_blk
        next_blk += 1
        
    # 4. Process regular files, allocate data blocks and inodes
    file_entries = [] # (parent_ino, name, child_ino, file_type)
    next_nid = 12
    
    def add_files_to_dir(parent_ino, files):
        nonlocal next_nid, next_blk
        for name, path, mode in files:
            if isinstance(path, (bytes, bytearray)):
                data = bytes(path)
            else:
                with open(path, 'rb') as f:
                    data = f.read()
            size = len(data)
            k = (size + BLOCK_SIZE - 1) // BLOCK_SIZE
            
            # Allocate data blocks
            file_data_blks = [next_blk + j for j in range(k)]
            next_blk += k
            
            # Write data blocks to image
            for j, daddr in enumerate(file_data_blks):
                start = daddr * BLOCK_SIZE
                chunk = data[j*BLOCK_SIZE : (j+1)*BLOCK_SIZE]
                image[start : start + len(chunk)] = chunk
                
            # Build addresses for inode direct/indirect mapping
            direct_limit = 923
            direct_blks = file_data_blks[:direct_limit]
            rem_blks = file_data_blks[direct_limit:]
            i_nids = [0, 0, 0, 0, 0]
            dnode_info = []
            
            if rem_blks:
                # Direct node 0 (i_nid[0])
                dnode_nid = next_nid
                next_nid += 1
                dnode_blkaddr = next_blk
                next_blk += 1
                i_nids[0] = dnode_nid
                
                dnode_blks = rem_blks[:1019]
                rem_blks = rem_blks[1019:]
                
                dnode_bytes = bytearray(4096)
                for idx, addr in enumerate(dnode_blks):
                    w32(dnode_bytes, idx * 4, addr)
                w32(dnode_bytes, NODE_FOOTER_OFF + 0, dnode_nid)
                w32(dnode_bytes, NODE_FOOTER_OFF + 4, dnode_nid)
                w32(dnode_bytes, NODE_FOOTER_OFF + 8, 0)
                w32(dnode_bytes, NODE_FOOTER_OFF + 12, 0)
                w32(dnode_bytes, NODE_FOOTER_OFF + 16, 1)
                
                dnode_info.append((dnode_nid, dnode_blkaddr, dnode_bytes))
                
            if rem_blks:
                # Direct node 1 (i_nid[1])
                dnode_nid = next_nid
                next_nid += 1
                dnode_blkaddr = next_blk
                next_blk += 1
                i_nids[1] = dnode_nid
                
                dnode_blks = rem_blks[:1019]
                rem_blks = rem_blks[1019:]
                
                dnode_bytes = bytearray(4096)
                for idx, addr in enumerate(dnode_blks):
                    w32(dnode_bytes, idx * 4, addr)
                w32(dnode_bytes, NODE_FOOTER_OFF + 0, dnode_nid)
                w32(dnode_bytes, NODE_FOOTER_OFF + 4, dnode_nid)
                w32(dnode_bytes, NODE_FOOTER_OFF + 8, 0)
                w32(dnode_bytes, NODE_FOOTER_OFF + 12, 0)
                w32(dnode_bytes, NODE_FOOTER_OFF + 16, 1)
                
                dnode_info.append((dnode_nid, dnode_blkaddr, dnode_bytes))
                
            if rem_blks:
                # Indirect node 0 (i_nid[2])
                ind_nid = next_nid
                next_nid += 1
                ind_blkaddr = next_blk
                next_blk += 1
                i_nids[2] = ind_nid
                
                ind_bytes = bytearray(4096)
                chunk_idx = 0
                while rem_blks:
                    chunk_blks = rem_blks[:1019]
                    rem_blks = rem_blks[1019:]
                    
                    dn_nid = next_nid
                    next_nid += 1
                    dn_blkaddr = next_blk
                    next_blk += 1
                    
                    dn_bytes = bytearray(4096)
                    for idx, addr in enumerate(chunk_blks):
                        w32(dn_bytes, idx * 4, addr)
                    w32(dn_bytes, NODE_FOOTER_OFF + 0, dn_nid)
                    w32(dn_bytes, NODE_FOOTER_OFF + 4, dn_nid)
                    w32(dn_bytes, NODE_FOOTER_OFF + 8, 0)
                    w32(dn_bytes, NODE_FOOTER_OFF + 12, 0)
                    w32(dn_bytes, NODE_FOOTER_OFF + 16, 1)
                    
                    dnode_info.append((dn_nid, dn_blkaddr, dn_bytes))
                    w32(ind_bytes, chunk_idx * 4, dn_nid)
                    chunk_idx += 1
                    
                w32(ind_bytes, NODE_FOOTER_OFF + 0, ind_nid)
                w32(ind_bytes, NODE_FOOTER_OFF + 4, ind_nid)
                w32(ind_bytes, NODE_FOOTER_OFF + 8, 0)
                w32(ind_bytes, NODE_FOOTER_OFF + 12, 0)
                w32(ind_bytes, NODE_FOOTER_OFF + 16, 1)
                
                dnode_info.append((ind_nid, ind_blkaddr, ind_bytes))
                
            if rem_blks:
                raise ValueError(f"File {name} is too large (> 4 GB), indirect blocks not supported!")
                
            # Allocate inode block
            file_inode_blk = next_blk
            next_blk += 1
            
            # Write inode block to image
            inode_bytes = bytearray(4096)
            w16(inode_bytes, 0, mode)
            w8 (inode_bytes, 2, 0)
            w8 (inode_bytes, 3, 0)
            w32(inode_bytes, 12, 1) # links
            w64(inode_bytes, 16, size)
            for idx, addr in enumerate(direct_blks):
                w32(inode_bytes, 364 + idx * 4, addr)
            for idx, nid in enumerate(i_nids):
                w32(inode_bytes, 4056 + idx * 4, nid)
            file_nid = next_nid
            next_nid += 1
            w32(inode_bytes, NODE_FOOTER_OFF + 0, file_nid)
            w32(inode_bytes, NODE_FOOTER_OFF + 4, file_nid)
            w32(inode_bytes, NODE_FOOTER_OFF + 8, 0)
            w32(inode_bytes, NODE_FOOTER_OFF + 12, 0)
            w32(inode_bytes, NODE_FOOTER_OFF + 16, 1)
            
            # Write blocks to image
            image[file_inode_blk * BLOCK_SIZE : (file_inode_blk + 1) * BLOCK_SIZE] = inode_bytes
            write_nat_entry(image, file_nid, file_inode_blk)
            
            for dn_nid, dn_blk, dn_bytes in dnode_info:
                image[dn_blk * BLOCK_SIZE : (dn_blk + 1) * BLOCK_SIZE] = dn_bytes
                write_nat_entry(image, dn_nid, dn_blk)
                
            file_entries.append((parent_ino, name.encode('utf-8'), file_nid, DT_REG))
            print(f"  Packed {name} (size: {size} bytes, inode blk: {file_inode_blk}, nid: {file_nid})")
            
    print("Packing binaries into /bin...")
    add_files_to_dir(4, bin_files)
    print("Packing libraries into /lib...")
    add_files_to_dir(11, lib_files)
    print("Packing files into /...")
    add_files_to_dir(3, root_files)
    print("Packing files into /etc...")
    add_files_to_dir(9, etc_files)
    
    # 5. Write directory blocks and inodes
    # Build dentry blocks list for each directory
    dentry_entries = {ino: [] for ino in dir_nodes.keys()}
    
    # Add . and .. to all directories
    dentry_entries[3].append((b'.', 3, DT_DIR))
    dentry_entries[3].append((b'..', 3, DT_DIR))
    dentry_entries[4].append((b'.', 4, DT_DIR))
    dentry_entries[4].append((b'..', 3, DT_DIR))
    dentry_entries[5].append((b'.', 5, DT_DIR))
    dentry_entries[5].append((b'..', 3, DT_DIR))
    dentry_entries[6].append((b'.', 6, DT_DIR))
    dentry_entries[6].append((b'..', 3, DT_DIR))
    dentry_entries[7].append((b'.', 7, DT_DIR))
    dentry_entries[7].append((b'..', 3, DT_DIR))
    dentry_entries[8].append((b'.', 8, DT_DIR))
    dentry_entries[8].append((b'..', 3, DT_DIR))
    dentry_entries[9].append((b'.', 9, DT_DIR))
    dentry_entries[9].append((b'..', 3, DT_DIR))
    dentry_entries[10].append((b'.', 10, DT_DIR))
    dentry_entries[10].append((b'..', 3, DT_DIR))
    dentry_entries[11].append((b'.', 11, DT_DIR))
    dentry_entries[11].append((b'..', 3, DT_DIR))
    
    # Add subdirectories to root /
    dentry_entries[3].append((b'bin', 4, DT_DIR))
    dentry_entries[3].append((b'old_root', 5, DT_DIR))
    dentry_entries[3].append((b'dev', 6, DT_DIR))
    dentry_entries[3].append((b'proc', 7, DT_DIR))
    dentry_entries[3].append((b'tmp', 8, DT_DIR))
    dentry_entries[3].append((b'etc', 9, DT_DIR))
    dentry_entries[3].append((b'mnt', 10, DT_DIR))
    dentry_entries[3].append((b'lib', 11, DT_DIR))
    
    # Add regular files to their directories
    for parent_ino, name, child_ino, ftype in file_entries:
        dentry_entries[parent_ino].append((name, child_ino, ftype))
        
    # Write directory data blocks and directory inodes
    for ino, name in dir_nodes.items():
        db_addr = data_blocks[ino]
        db_bytes = build_dentry_block(dentry_entries[ino])
        image[db_addr * BLOCK_SIZE : (db_addr + 1) * BLOCK_SIZE] = db_bytes
        
        in_addr = inode_blocks[ino]
        # Inodes point to their data block
        mode = 0o040755
        links = 2 if ino != 3 else 2 + 8 # Root has . and .. and 8 subdirs
        if ino == 3:
            links = 2 + 8
        else:
            links = 2
            
        inode_bytes = build_inode_block(mode, links, BLOCK_SIZE, [db_addr], ino)
        image[in_addr * BLOCK_SIZE : (in_addr + 1) * BLOCK_SIZE] = inode_bytes
        
        write_nat_entry(image, ino, in_addr)
        
    # 6. SIT (Segment Info Table) Updates
    MAIN_START_SEG = MAIN_BLKADDR // BLOCKS_PER_SEG
    valid_blocks_per_seg = [0] * main_segs
    valid_map_per_seg = [bytearray(64) for _ in range(main_segs)]
    
    # Count blocks allocated in main area
    allocated_main_blocks = next_blk - MAIN_BLKADDR
    for b in range(allocated_main_blocks):
        seg = b // BLOCKS_PER_SEG
        blk = b % BLOCKS_PER_SEG
        valid_blocks_per_seg[seg] += 1
        valid_map_per_seg[seg][blk // 8] |= (1 << (blk % 8))
        
    # All meta segments fully used
    for seg in range(MAIN_START_SEG):
        o = (SIT_BLKADDR + seg // SIT_PER_BLK) * BLOCK_SIZE + (seg % SIT_PER_BLK) * SIT_ENTRY_SIZE
        w16(image, o, BLOCKS_PER_SEG)
        for b in range(64): image[o + 2 + b] = 0xFF
        
    # Main segments
    for seg in range(main_segs):
        o = (SIT_BLKADDR + (MAIN_START_SEG + seg) // SIT_PER_BLK) * BLOCK_SIZE + ((MAIN_START_SEG + seg) % SIT_PER_BLK) * SIT_ENTRY_SIZE
        w16(image, o, valid_blocks_per_seg[seg])
        image[o + 2 : o + 66] = valid_map_per_seg[seg]
        
    # 7. Checkpoint updates
    valid_blocks_count = next_blk - MAIN_BLKADDR
    valid_nodes_count = next_nid - 3
    used_segs = (valid_blocks_count + BLOCKS_PER_SEG - 1) // BLOCKS_PER_SEG
    free_segs = main_segs - used_segs

    # The runtime allocator (servers/f2fs/src/lib.rs) starts handing out new
    # node/data blocks from these curseg positions and bumps them one block at
    # a time — it never consults the SIT valid-map to skip already-used blocks.
    # They must therefore start on fresh, fully-empty segments *past* every
    # block written above (not segment 0/1, which hold real directories and
    # files), and node/data need distinct segments so their independent
    # cursors can't collide with each other.
    first_free_seg = used_segs
    cur_node_segno, cur_data_segno = first_free_seg, first_free_seg + 1
    assert cur_data_segno < main_segs, "not enough reserved segments for runtime node/data logs"

    write_checkpoint(image, CP_BLKADDR, 1, valid_blocks_count, valid_nodes_count, free_segs, next_nid, main_segs,
                      cur_node_segno, 0, cur_data_segno, 0)
    write_checkpoint(image, CP_BLKADDR + BLOCKS_PER_SEG, 0, valid_blocks_count, valid_nodes_count, free_segs, next_nid, main_segs,
                      cur_node_segno, 0, cur_data_segno, 0)
    
    # Write output file
    with open(out_file, 'wb') as f:
        f.write(image)
        
    print(f"🎉 Created populated F2FS image at {out_file} (size: {len(image)} bytes)")
    print(f"   Allocated {valid_blocks_count} blocks starting at MAIN_BLKADDR {MAIN_BLKADDR}")

if __name__ == '__main__':
    main()
