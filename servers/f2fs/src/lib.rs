//! F2FS filesystem server — multi-instance, one IPC port per mounted volume.
//!
//! Each call to `mount()` spawns an independent server that:
//!   1. Reads the F2FS superblock/checkpoint from a VirtIO block device index
//!   2. Registers with VFS at the requested mount point
//!   3. Handles VFS_OPEN/READ/WRITE/CLOSE/STAT/GETDENTS64/MKDIR/UNLINK/FTRUNCATE
//!
//! Disk format assumption: created with `mkfs.f2fs -O ^extra_attr,^inline_data,^inline_dentry`
//! so all inodes use standard block pointers and all directories use regular dentry blocks.

#![no_std]

extern crate alloc;
extern crate mm;

use ipc::{Message, port};
use spin::Mutex;
use drivers::blkdev as virtio_blk;

// ── VFS protocol constants ────────────────────────────────────────────────────

const VFS_OPEN:       u64 = 0x10;
const VFS_READ:       u64 = 0x11;
const VFS_WRITE:      u64 = 0x12;
const VFS_CLOSE:      u64 = 0x13;
const VFS_STAT:       u64 = 0x14;
const VFS_LSEEK:      u64 = 0x15;
const VFS_GETDENTS64: u64 = 0x1D;
const VFS_UNLINK:     u64 = 0x1F;
const VFS_MKDIR:      u64 = 0x20;
const VFS_FTRUNCATE:  u64 = 0x21;
const VFS_RENAME:     u64 = 0x22;
const VFS_RMDIR:      u64 = 0x29;
const VFS_STATFS:     u64 = 0x33;
const VFS_SYMLINK:    u64 = 0x35;
const VFS_FD_PATH:    u64 = 0x23;
const VFS_READLINK:   u64 = 0x36;
const VFS_LINK:       u64 = 0x37;
const VFS_LSTAT:      u64 = 0x38;
const VFS_CHMOD:      u64 = 0x2B;
const VFS_FCHMOD:     u64 = 0x2C;
const VFS_CHOWN:      u64 = 0x2D;
const VFS_FCHOWN:     u64 = 0x2E;

const O_WRONLY:  u64 = 1;
const O_RDWR:    u64 = 2;
const O_CREAT:   u64 = 0o100;
const O_EXCL:    u64 = 0o200;
const O_TRUNC:   u64 = 0o1000;

// ── F2FS on-disk byte-offset constants ────────────────────────────────────────

const BLOCK_SIZE: usize = 4096;
const F2FS_MAGIC: u32 = 0xF2F5_2010;
const F2FS_SB_OFFSET: usize = 1024; // within first block

// Superblock offsets (relative to F2FS_SB_OFFSET within block 0)
const SB_MAGIC:            usize = 0;
const SB_LOG_BLK_PER_SEG:  usize = 20;
const SB_SEG_CNT_CKPT:     usize = 52;
const SB_SEG_CNT_NAT:       usize = 60;
const SB_CP_BLKADDR:        usize = 76;
const SB_SIT_BLKADDR:       usize = 80;
const SB_NAT_BLKADDR:       usize = 84;
const SB_MAIN_BLKADDR:      usize = 92;
const SB_ROOT_INO:          usize = 96;

// Checkpoint offsets (within a checkpoint block)
const CP_VER:            usize = 0;
const CP_FREE_SEG_CNT:   usize = 32;
const CP_CUR_NODE_SEGNO: usize = 36;   // u32 [0] of node log
const CP_CUR_NODE_BLKOFF:usize = 68;   // u16 [0] of node log
const CP_CUR_DATA_SEGNO: usize = 84;   // u32 [0] of data log
const CP_CUR_DATA_BLKOFF:usize = 116;  // u16 [0] of data log
const CP_PACK_TOTAL:     usize = 136;
const CP_NEXT_FREE_NID:  usize = 152;

// Inode (node block) field offsets
const INO_MODE:      usize = 0;
const INO_INLINE:    usize = 3;
// `__le32 i_uid` / `__le32 i_gid` — real f2fs_inode layout puts these
// immediately after i_mode/i_advise/i_inline and before i_links (offset 12).
// scripts/mkfs-f2fs-populated.py never writes these bytes, so every packed
// inode starts at uid=0/gid=0 (root), matching what stat_common used to
// hardcode before chown could actually persist anything.
const INO_UID:       usize = 4;
const INO_GID:       usize = 8;
const INO_LINKS:     usize = 12;
const INO_SIZE:      usize = 16;
const INO_NAMELEN:   usize = 88;
const INO_NAME:      usize = 92;   // [u8; 255]
// The union (i_addr / extra-attrs) starts here:
const INODE_UNION:   usize = 364;
// i_nid[5] start, footer at NODE_FOOTER_OFF
const INODE_NIDS_OFF:usize = 4056;
const NODE_FOOTER_OFF:usize = 4076; // footer = 5×u32 = 20 bytes

// F2FS_INLINE flags
const F2FS_EXTRA_ATTR:    u8 = 0x20;

// NAT
const NAT_ENTRY_SIZE:     usize = 9;   // version(1) + ino(4) + blkaddr(4)
const NAT_ENTRY_PER_BLK:  usize = 4096 / NAT_ENTRY_SIZE; // 455

// SIT: struct f2fs_sit_entry = vblocks(2) + valid_map(64) + mtime(8) = 74 bytes
const SIT_ENTRY_SIZE:     usize = 74;
const SIT_PER_BLK:        usize = 4096 / SIT_ENTRY_SIZE; // 55
const SIT_VMAP_OFF:       usize = 2;   // valid_map offset within sit entry
const SIT_VBLOCKS_MASK:   u16   = 0x03FF;

// Directory block constants (f2fs_dentry_block)
const NR_DENTRY_IN_BLK:   usize = 214;
const DENTRY_BITMAP_SIZE: usize = 27;   // ceil(214/8)
const DENTRY_RESERVED:    usize = 3;
const DENTRY_ENTRIES_OFF: usize = DENTRY_BITMAP_SIZE + DENTRY_RESERVED; // 30
const DENTRY_SLOT_LEN:    usize = 8;
const DENTRY_NAMES_OFF:   usize = DENTRY_ENTRIES_OFF + NR_DENTRY_IN_BLK * 11; // 30+2354=2384
const DENTRY_ENTRY_SIZE:  usize = 11;

// Directory-entry type byte. This volume format stores Linux's DT_* values
// directly in the dentry (not the F2FS_FT_* enum), because that is what
// scripts/mkfs-f2fs-populated.py writes and what handle_getdents hands
// straight back as `d_type`. Keep the three in lockstep.
const DT_DIR:  u8 = 4;
const DT_REG:  u8 = 8;
const DT_LNK:  u8 = 10;

// File mode bits
const S_IFMT:  u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;
const S_IFLNK: u16 = 0o120000;

/// Symlink traversals allowed in one path resolution before the walk gives up.
/// Matches Linux's MAXSYMLINKS and the VFS server's `SYMLINK_MAX_HOPS`; the
/// walk below is iterative, so a cycle costs 40 bounded passes and no stack.
const SYMLINK_MAX_HOPS: u32 = 40;

// ── Byte helpers ──────────────────────────────────────────────────────────────

#[inline] fn r16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off+1]])
}
#[inline] fn r32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off+4].try_into().unwrap())
}
#[inline] fn r64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off+8].try_into().unwrap())
}
#[inline] fn w16(b: &mut [u8], off: usize, v: u16) { b[off..off+2].copy_from_slice(&v.to_le_bytes()); }
#[inline] fn w32(b: &mut [u8], off: usize, v: u32) { b[off..off+4].copy_from_slice(&v.to_le_bytes()); }
#[inline] fn w64(b: &mut [u8], off: usize, v: u64) { b[off..off+8].copy_from_slice(&v.to_le_bytes()); }

fn inode_addr_base(blk: &[u8]) -> usize {
    if blk[INO_INLINE] & F2FS_EXTRA_ATTR != 0 {
        let extra_sz = r16(blk, INODE_UNION) as usize;
        let slots = (extra_sz / 4).min(64);
        INODE_UNION + slots * 4
    } else {
        INODE_UNION
    }
}

fn inode_get_blkaddr(blk: &[u8], idx: usize) -> u32 {
    let base = inode_addr_base(blk);
    let off = base + idx * 4;
    if off + 4 > INODE_NIDS_OFF { return 0; }
    r32(blk, off)
}

fn inode_set_blkaddr(blk: &mut [u8], idx: usize, addr: u32) {
    let base = inode_addr_base(blk);
    let off = base + idx * 4;
    if off + 4 > INODE_NIDS_OFF { return; }
    w32(blk, off, addr);
}

fn inode_max_direct(blk: &[u8]) -> usize {
    let base = inode_addr_base(blk);
    if INODE_NIDS_OFF <= base { return 0; }
    (INODE_NIDS_OFF - base) / 4
}

fn inode_get_nid(blk: &[u8], n: usize) -> u32 {
    r32(blk, INODE_NIDS_OFF + n * 4)
}

fn inode_set_nid(blk: &mut [u8], n: usize, val: u32) {
    w32(blk, INODE_NIDS_OFF + n * 4, val);
}

// In a direct_node block, the 1018 block addresses start at offset 0.
fn dnode_get_blkaddr(blk: &[u8], idx: usize) -> u32 {
    let off = idx * 4;
    if off + 4 > NODE_FOOTER_OFF { return 0; }
    r32(blk, off)
}

fn dnode_set_blkaddr(blk: &mut [u8], idx: usize, addr: u32) {
    let off = idx * 4;
    if off + 4 > NODE_FOOTER_OFF { return; }
    w32(blk, off, addr);
}

// ── Block cache ───────────────────────────────────────────────────────────────

// 4 slots × 4 KB = 16 KB per mount — keeps MountState small enough to construct
// on the 64 KB kernel boot stack without overflowing and corrupting statics.
const CACHE_SLOTS: usize = 4;
const NULL_BLK: u64 = u64::MAX;

#[derive(Clone, Copy)]
struct CacheEntry {
    blk_no: u64,
    dirty: bool,
    lru: u32,
    data: [u8; BLOCK_SIZE],
}

impl CacheEntry {
    const fn empty() -> Self {
        Self { blk_no: NULL_BLK, dirty: false, lru: 0, data: [0u8; BLOCK_SIZE] }
    }
}

struct BlockCache {
    slots: [CacheEntry; CACHE_SLOTS],
    tick: u32,
}

impl BlockCache {
    const fn new() -> Self {
        Self { slots: [const { CacheEntry::empty() }; CACHE_SLOTS], tick: 0 }
    }

    fn find(&self, blk: u64) -> Option<usize> {
        self.slots.iter().position(|e| e.blk_no == blk)
    }

    fn evict_slot(&mut self) -> usize {
        // LRU eviction; never evict dirty (caller flushes first or we flush here)
        let (idx, _) = self.slots.iter().enumerate()
            .min_by_key(|(_, e)| if e.dirty { u32::MAX } else { e.lru })
            .unwrap_or((0, &self.slots[0]));
        idx
    }

    fn read(&mut self, dev: usize, blk: u64) -> &[u8; BLOCK_SIZE] {
        self.tick = self.tick.wrapping_add(1);
        if let Some(i) = self.find(blk) {
            self.slots[i].lru = self.tick;
            // SAFETY: returning reference to array inside slot; no aliasing since caller has &mut
            return unsafe { &*(self.slots[i].data.as_ptr() as *const [u8; BLOCK_SIZE]) };
        }
        let i = self.evict_slot();
        if self.slots[i].dirty {
            let blk_no = self.slots[i].blk_no;
            let data = self.slots[i].data;
            virtio_blk::write_block(dev, blk_no, &data);
            self.slots[i].dirty = false;
        }
        let mut buf = [0u8; BLOCK_SIZE];
        virtio_blk::read_block(dev, blk, &mut buf);
        self.slots[i] = CacheEntry { blk_no: blk, dirty: false, lru: self.tick, data: buf };
        unsafe { &*(self.slots[i].data.as_ptr() as *const [u8; BLOCK_SIZE]) }
    }

    fn write(&mut self, dev: usize, blk: u64, src: &[u8; BLOCK_SIZE]) {
        self.tick = self.tick.wrapping_add(1);
        let i = if let Some(i) = self.find(blk) { i } else {
            let i = self.evict_slot();
            if self.slots[i].dirty {
                let bn = self.slots[i].blk_no;
                let d = self.slots[i].data;
                virtio_blk::write_block(dev, bn, &d);
            }
            i
        };
        self.slots[i] = CacheEntry { blk_no: blk, dirty: true, lru: self.tick, data: *src };
    }

    fn flush_all(&mut self, dev: usize) {
        for e in self.slots.iter_mut() {
            if e.dirty {
                let d = e.data;
                virtio_blk::write_block(dev, e.blk_no, &d);
                e.dirty = false;
            }
        }
    }

    fn get_mut(&mut self, dev: usize, blk: u64) -> &mut [u8; BLOCK_SIZE] {
        self.tick = self.tick.wrapping_add(1);
        if self.find(blk).is_none() {
            let i = self.evict_slot();
            if self.slots[i].dirty {
                let bn = self.slots[i].blk_no;
                let d = self.slots[i].data;
                virtio_blk::write_block(dev, bn, &d);
            }
            let mut buf = [0u8; BLOCK_SIZE];
            virtio_blk::read_block(dev, blk, &mut buf);
            let idx = i;
            self.slots[idx] = CacheEntry { blk_no: blk, dirty: false, lru: self.tick, data: buf };
        }
        let i = self.find(blk).unwrap();
        self.slots[i].dirty = true;
        self.slots[i].lru = self.tick;
        unsafe { &mut *(self.slots[i].data.as_mut_ptr() as *mut [u8; BLOCK_SIZE]) }
    }
}

// ── Superblock info (parsed) ──────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct SbInfo {
    blocks_per_seg: u32,
    seg_cnt_ckpt: u32,
    seg_cnt_nat: u32,
    cp_blkaddr: u32,
    sit_blkaddr: u32,
    nat_blkaddr: u32,
    main_blkaddr: u32,
    root_ino: u32,
}

impl SbInfo {
    fn parse(block0: &[u8; BLOCK_SIZE]) -> Option<Self> {
        let sb = &block0[F2FS_SB_OFFSET..];
        if r32(sb, SB_MAGIC) != F2FS_MAGIC { return None; }
        let log_bps = r32(sb, SB_LOG_BLK_PER_SEG);
        Some(Self {
            blocks_per_seg:  1u32 << log_bps,
            seg_cnt_ckpt:    r32(sb, SB_SEG_CNT_CKPT),
            seg_cnt_nat:     r32(sb, SB_SEG_CNT_NAT),
            cp_blkaddr:      r32(sb, SB_CP_BLKADDR),
            sit_blkaddr:     r32(sb, SB_SIT_BLKADDR),
            nat_blkaddr:     r32(sb, SB_NAT_BLKADDR),
            main_blkaddr:    r32(sb, SB_MAIN_BLKADDR),
            root_ino:        r32(sb, SB_ROOT_INO),
        })
    }
}

// ── Checkpoint info (parsed) ──────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct CpInfo {
    ver:              u64,
    free_seg_cnt:     u32,
    cur_node_segno:   u32,
    cur_node_blkoff:  u16,
    cur_data_segno:   u32,
    cur_data_blkoff:  u16,
    pack_total:       u32,
    next_free_nid:    u32,
    active_pack:      u8,  // 0 or 1
}

impl CpInfo {
    fn parse_pack(blk: &[u8; BLOCK_SIZE]) -> (u64, Self) {
        let ver = r64(blk, CP_VER);
        let cp = CpInfo {
            ver,
            free_seg_cnt:    r32(blk, CP_FREE_SEG_CNT),
            cur_node_segno:  r32(blk, CP_CUR_NODE_SEGNO),
            cur_node_blkoff: r16(blk, CP_CUR_NODE_BLKOFF),
            cur_data_segno:  r32(blk, CP_CUR_DATA_SEGNO),
            cur_data_blkoff: r16(blk, CP_CUR_DATA_BLKOFF),
            pack_total:      r32(blk, CP_PACK_TOTAL),
            next_free_nid:   r32(blk, CP_NEXT_FREE_NID),
            active_pack:     0,
        };
        (ver, cp)
    }
}

// ── Open file table ───────────────────────────────────────────────────────────

const MAX_OPEN_FILES: usize = 32;

/// Longest absolute path remembered per open file, for `VFS_FD_PATH`
/// (`readlink("/proc/self/fd/N")`). Sized to fit any path the kernel's
/// `KPATH_MAX`-bounded resolver can hand us; longer ones are simply not
/// recovered rather than truncated into a lie.
const MAX_OPEN_PATH: usize = 192;

#[derive(Clone, Copy)]

struct OpenFile {
    inode:    u32,
    pos:      u64,
    writable: bool,
    in_use:   bool,
    /// The absolute path this fd was opened by. The kernel resolves every
    /// path syscall against the caller's cwd before the VFS ever sees it
    /// (`resolve_user_path` in kernel/src/syscall.rs), so what arrives here
    /// is already absolute and normalised — which is exactly what
    /// `/proc/self/fd/N` has to report. Nothing else can reconstruct it: the
    /// slot otherwise holds only an inode number, and F2FS has no
    /// inode→dentry reverse map.
    path:     [u8; MAX_OPEN_PATH],
    path_len: usize,
}

impl OpenFile {
    const fn empty() -> Self {
        Self { inode: 0, pos: 0, writable: false, in_use: false,
               path: [0; MAX_OPEN_PATH], path_len: 0 }
    }
}

// ── Per-mount state ───────────────────────────────────────────────────────────

struct MountState {
    dev:          usize,
    port:         u32,
    mount_prefix: &'static str,
    sb:           SbInfo,
    cp:           CpInfo,
    dirty_writes: u32,
    open_files:   [OpenFile; MAX_OPEN_FILES],
    cache:        BlockCache,
}

const MAX_MOUNTS: usize = 8;

static F2FS_MOUNTS: Mutex<[Option<MountState>; MAX_MOUNTS]> =
    Mutex::new([const { None }; MAX_MOUNTS]);

// ── Reply helpers ─────────────────────────────────────────────────────────────

fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off+8].try_into().unwrap_or([0u8; 8]))
}
fn ok_reply()         -> Message { make_reply(0) }
fn err_reply(e: i32)  -> Message { make_reply(e as i64) }
fn val_reply(v: u64)  -> Message { make_reply(v as i64) }
fn make_reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

// ── NAT operations ────────────────────────────────────────────────────────────

fn nat_lookup(ms: &mut MountState, ino: u32) -> u32 {
    let nat_blk = ms.sb.nat_blkaddr + ino / NAT_ENTRY_PER_BLK as u32;
    let idx     = (ino % NAT_ENTRY_PER_BLK as u32) as usize;
    let blk     = ms.cache.read(ms.dev, nat_blk as u64);
    let off     = idx * NAT_ENTRY_SIZE + 5; // +1 version +4 ino_field → +5 for blkaddr
    r32(blk, off)
}

fn nat_update(ms: &mut MountState, ino: u32, blk_addr: u32) {
    let nat_blk = ms.sb.nat_blkaddr + ino / NAT_ENTRY_PER_BLK as u32;
    let idx     = (ino % NAT_ENTRY_PER_BLK as u32) as usize;
    let off     = idx * NAT_ENTRY_SIZE;
    let blk = ms.cache.get_mut(ms.dev, nat_blk as u64);
    blk[off] = 1;                          // version bump
    w32(blk, off + 1, ino);               // ino
    w32(blk, off + 5, blk_addr);          // block_addr
}

// ── SIT: find a free segment ──────────────────────────────────────────────────

fn sit_find_free_seg(ms: &mut MountState, after: u32) -> Option<u32> {
    let _main_segs = ms.sb.main_blkaddr; // count derived from sit
    // Scan SIT blocks for a segment with 0 valid blocks
    let _sit_total_blks = ms.sb.seg_cnt_nat; // actually seg_cnt_main, but use what's available
    // Best approach: scan through SIT entries linearly starting from after+1
    let start_seg = after + 1;
    // Use a reasonable upper bound (scan up to 1024 segments)
    for seg_off in 0..1024u32 {
        let seg = (start_seg + seg_off) % 1024;
        let sit_blk_idx = seg / SIT_PER_BLK as u32;
        let sit_entry   = (seg % SIT_PER_BLK as u32) as usize;
        let sit_blkno   = ms.sb.sit_blkaddr + sit_blk_idx;
        let sit_blk = ms.cache.read(ms.dev, sit_blkno as u64);
        let entry_off = sit_entry * SIT_ENTRY_SIZE;
        if entry_off + 2 > BLOCK_SIZE { continue; }
        let vblocks = r16(sit_blk, entry_off) & SIT_VBLOCKS_MASK;
        if vblocks == 0 {
            return Some(seg);
        }
    }
    None
}

fn sit_mark_block_used(ms: &mut MountState, seg: u32, blk_in_seg: u32) {
    let sit_blk_idx = seg / SIT_PER_BLK as u32;
    let sit_entry   = (seg % SIT_PER_BLK as u32) as usize;
    let sit_blkno   = ms.sb.sit_blkaddr + sit_blk_idx;
    let blk = ms.cache.get_mut(ms.dev, sit_blkno as u64);
    let entry_off = sit_entry * SIT_ENTRY_SIZE;
    // Bump vblocks count
    let v = r16(blk, entry_off);
    let cnt = (v & SIT_VBLOCKS_MASK) + 1;
    let new_v = (v & !SIT_VBLOCKS_MASK) | (cnt & SIT_VBLOCKS_MASK);
    w16(blk, entry_off, new_v);
    // Set bit in valid_map
    let byte_idx = blk_in_seg as usize / 8;
    let bit      = blk_in_seg as usize % 8;
    if entry_off + SIT_VMAP_OFF + byte_idx < BLOCK_SIZE {
        blk[entry_off + SIT_VMAP_OFF + byte_idx] |= 1 << bit;
    }
}

// ── Log-structured block allocator ───────────────────────────────────────────

fn alloc_data_block(ms: &mut MountState) -> Option<u32> {
    let bps = ms.sb.blocks_per_seg;
    if ms.cp.cur_data_blkoff as u32 >= bps {
        let next_seg = sit_find_free_seg(ms, ms.cp.cur_data_segno)?;
        ms.cp.cur_data_segno = next_seg;
        ms.cp.cur_data_blkoff = 0;
    }
    let seg   = ms.cp.cur_data_segno;
    let blkoff = ms.cp.cur_data_blkoff;
    let phys  = ms.sb.main_blkaddr + seg * bps + blkoff as u32;
    sit_mark_block_used(ms, seg, blkoff as u32);
    ms.cp.cur_data_blkoff += 1;
    Some(phys)
}

fn alloc_node_block(ms: &mut MountState) -> Option<u32> {
    let bps = ms.sb.blocks_per_seg;
    if ms.cp.cur_node_blkoff as u32 >= bps {
        let next_seg = sit_find_free_seg(ms, ms.cp.cur_node_segno)?;
        ms.cp.cur_node_segno = next_seg;
        ms.cp.cur_node_blkoff = 0;
    }
    let seg   = ms.cp.cur_node_segno;
    let blkoff = ms.cp.cur_node_blkoff;
    let phys  = ms.sb.main_blkaddr + seg * bps + blkoff as u32;
    sit_mark_block_used(ms, seg, blkoff as u32);
    ms.cp.cur_node_blkoff += 1;
    Some(phys)
}

// ── Checkpoint flush ──────────────────────────────────────────────────────────

fn flush_checkpoint(ms: &mut MountState) {
    ms.cache.flush_all(ms.dev);

    let bps = ms.sb.blocks_per_seg;
    // CP area: two packs of (seg_cnt_ckpt/2 * blocks_per_seg) blocks each
    let pack_size = (ms.sb.seg_cnt_ckpt / 2) * bps;
    let inactive = if ms.cp.active_pack == 0 { 1u32 } else { 0u32 };
    let cp_blkno = ms.sb.cp_blkaddr + inactive * pack_size;

    ms.cp.ver = ms.cp.ver.wrapping_add(1);
    ms.cp.active_pack = inactive as u8;

    let mut buf = [0u8; BLOCK_SIZE];
    w64(&mut buf, CP_VER,             ms.cp.ver);
    w32(&mut buf, CP_FREE_SEG_CNT,    ms.cp.free_seg_cnt);
    w32(&mut buf, CP_CUR_NODE_SEGNO,  ms.cp.cur_node_segno);
    w16(&mut buf, CP_CUR_NODE_BLKOFF, ms.cp.cur_node_blkoff);
    w32(&mut buf, CP_CUR_DATA_SEGNO,  ms.cp.cur_data_segno);
    w16(&mut buf, CP_CUR_DATA_BLKOFF, ms.cp.cur_data_blkoff);
    w32(&mut buf, CP_PACK_TOTAL,      ms.cp.pack_total.max(1));
    w32(&mut buf, CP_NEXT_FREE_NID,   ms.cp.next_free_nid);
    virtio_blk::write_block(ms.dev, cp_blkno as u64, &buf);

    ms.dirty_writes = 0;
}

fn maybe_flush(ms: &mut MountState) {
    ms.dirty_writes += 1;
    if ms.dirty_writes >= 16 {
        flush_checkpoint(ms);
    }
}

// ── Inode operations ──────────────────────────────────────────────────────────

fn inode_size(blk: &[u8]) -> u64 { r64(blk, INO_SIZE) }
fn inode_mode(blk: &[u8]) -> u16 { r16(blk, INO_MODE) }
fn inode_uid(blk: &[u8]) -> u32 { r32(blk, INO_UID) }
fn inode_gid(blk: &[u8]) -> u32 { r32(blk, INO_GID) }
fn inode_links(blk: &[u8]) -> u32 { r32(blk, INO_LINKS) }
fn inode_is_dir(blk: &[u8]) -> bool { (inode_mode(blk) & S_IFMT) == S_IFDIR }

/// Allocate and initialize a new inode block; returns (ino, phys_blkaddr).
fn create_inode(ms: &mut MountState, mode: u16, parent_ino: u32, name: &[u8]) -> Option<(u32, u32)> {
    let ino = ms.cp.next_free_nid;
    ms.cp.next_free_nid = ino.wrapping_add(1);

    let phys = alloc_node_block(ms)?;
    let mut buf = [0u8; BLOCK_SIZE];

    w16(&mut buf, INO_MODE,    mode);
    w32(&mut buf, INO_LINKS,   1);
    w64(&mut buf, INO_SIZE,    0);
    w32(&mut buf, 84, parent_ino); // i_pino
    let namelen = name.len().min(255) as u32;
    w32(&mut buf, INO_NAMELEN, namelen);
    buf[INO_NAME..INO_NAME + namelen as usize].copy_from_slice(&name[..namelen as usize]);

    // node footer: nid + ino
    w32(&mut buf, NODE_FOOTER_OFF,     ino);
    w32(&mut buf, NODE_FOOTER_OFF + 4, ino);

    ms.cache.write(ms.dev, phys as u64, &buf);
    nat_update(ms, ino, phys);

    Some((ino, phys))
}

// ── Data block access ─────────────────────────────────────────────────────────

/// Map logical block index `idx` to physical block address using inode.
/// Returns 0 for unallocated (sparse/hole) blocks.
fn inode_logical_to_phys(ms: &mut MountState, ino: u32, idx: u64) -> u32 {
    let iblkaddr = nat_lookup(ms, ino);
    let iblk = ms.cache.read(ms.dev, iblkaddr as u64);
    let iblk = unsafe { &*(iblk as *const [u8; BLOCK_SIZE]) }; // detach lifetime

    let max_direct = inode_max_direct(iblk) as u64;
    if idx < max_direct {
        return inode_get_blkaddr(iblk, idx as usize);
    }

    let mut rem = idx - max_direct;
    const ADDRS_PER_DNODE: u64 = (NODE_FOOTER_OFF / 4) as u64; // 1019
    const NIDS_PER_BLOCK: u64 = (NODE_FOOTER_OFF / 4) as u64;  // 1019

    // Direct node 0: i_nid[0]
    if rem < ADDRS_PER_DNODE {
        let nid = inode_get_nid(iblk, 0);
        if nid == 0 { return 0; }
        let dblkaddr = nat_lookup(ms, nid);
        let dblk = ms.cache.read(ms.dev, dblkaddr as u64);
        return dnode_get_blkaddr(dblk, rem as usize);
    }
    rem -= ADDRS_PER_DNODE;

    // Direct node 1: i_nid[1]
    if rem < ADDRS_PER_DNODE {
        let nid = inode_get_nid(iblk, 1);
        if nid == 0 { return 0; }
        let dblkaddr = nat_lookup(ms, nid);
        let dblk = ms.cache.read(ms.dev, dblkaddr as u64);
        return dnode_get_blkaddr(dblk, rem as usize);
    }
    rem -= ADDRS_PER_DNODE;

    // Indirect node 0: i_nid[2]
    let blocks_per_indirect = NIDS_PER_BLOCK * ADDRS_PER_DNODE;
    if rem < blocks_per_indirect {
        let nid = inode_get_nid(iblk, 2);
        if nid == 0 { return 0; }
        let ind_blkaddr = nat_lookup(ms, nid);
        let ind_blk = ms.cache.read(ms.dev, ind_blkaddr as u64);
        let dnode_idx = rem / ADDRS_PER_DNODE;
        let dnode_off = rem % ADDRS_PER_DNODE;
        let dnid = dnode_get_blkaddr(ind_blk, dnode_idx as usize);
        if dnid == 0 { return 0; }
        let dblkaddr = nat_lookup(ms, dnid);
        let dblk = ms.cache.read(ms.dev, dblkaddr as u64);
        return dnode_get_blkaddr(dblk, dnode_off as usize);
    }
    rem -= blocks_per_indirect;

    // Indirect node 1: i_nid[3]
    if rem < blocks_per_indirect {
        let nid = inode_get_nid(iblk, 3);
        if nid == 0 { return 0; }
        let ind_blkaddr = nat_lookup(ms, nid);
        let ind_blk = ms.cache.read(ms.dev, ind_blkaddr as u64);
        let dnode_idx = rem / ADDRS_PER_DNODE;
        let dnode_off = rem % ADDRS_PER_DNODE;
        let dnid = dnode_get_blkaddr(ind_blk, dnode_idx as usize);
        if dnid == 0 { return 0; }
        let dblkaddr = nat_lookup(ms, dnid);
        let dblk = ms.cache.read(ms.dev, dblkaddr as u64);
        return dnode_get_blkaddr(dblk, dnode_off as usize);
    }
    rem -= blocks_per_indirect;

    // Double indirect node: i_nid[4]
    let blocks_per_dindirect = NIDS_PER_BLOCK * blocks_per_indirect;
    if rem < blocks_per_dindirect {
        let nid = inode_get_nid(iblk, 4);
        if nid == 0 { return 0; }
        let dind_blkaddr = nat_lookup(ms, nid);
        let dind_blk = ms.cache.read(ms.dev, dind_blkaddr as u64);
        let ind_idx = rem / blocks_per_indirect;
        let ind_rem = rem % blocks_per_indirect;
        let ind_nid = dnode_get_blkaddr(dind_blk, ind_idx as usize);
        if ind_nid == 0 { return 0; }
        let ind_blkaddr = nat_lookup(ms, ind_nid);
        let ind_blk = ms.cache.read(ms.dev, ind_blkaddr as u64);
        let dnode_idx = ind_rem / ADDRS_PER_DNODE;
        let dnode_off = ind_rem % ADDRS_PER_DNODE;
        let dnid = dnode_get_blkaddr(ind_blk, dnode_idx as usize);
        if dnid == 0 { return 0; }
        let dblkaddr = nat_lookup(ms, dnid);
        let dblk = ms.cache.read(ms.dev, dblkaddr as u64);
        return dnode_get_blkaddr(dblk, dnode_off as usize);
    }

    0
}

/// Read `count` bytes from file `ino` at `pos` into `buf`.
fn read_file_data(ms: &mut MountState, ino: u32, pos: u64, buf: *mut u8, count: usize) -> usize {
    let iblkaddr = nat_lookup(ms, ino);
    let iblk_copy = {
        let b = ms.cache.read(ms.dev, iblkaddr as u64);
        let mut c = [0u8; BLOCK_SIZE]; c.copy_from_slice(b); c
    };
    let fsize = inode_size(&iblk_copy);
    if pos >= fsize { return 0; }
    let readable = (fsize - pos).min(count as u64) as usize;
    let mut done = 0usize;

    while done < readable {
        let file_off = pos + done as u64;
        let blk_idx  = file_off / BLOCK_SIZE as u64;
        let blk_off  = (file_off % BLOCK_SIZE as u64) as usize;
        let phys = inode_logical_to_phys(ms, ino, blk_idx);
        let chunk = (BLOCK_SIZE - blk_off).min(readable - done);

        // Fast path: a full-block-aligned span. Gather the run of blocks that
        // are *physically* contiguous on the device and fetch it with one
        // multi-block virtio request straight into the destination buffer.
        // This bypasses the LRU block cache, which (a) large sequential reads
        // would otherwise evict entirely and (b) costs one device round trip
        // per 4 KiB. Any block already present in the cache ends the run so a
        // dirty (not yet flushed) copy is never shadowed by stale device data.
        if blk_off == 0 && chunk == BLOCK_SIZE && phys != 0
            && ms.cache.find(phys as u64).is_none()
        {
            let max_run = (readable - done) / BLOCK_SIZE;
            let mut run = 1usize;
            while run < max_run {
                let next = inode_logical_to_phys(ms, ino, blk_idx + run as u64);
                if next == 0 || next as u64 != phys as u64 + run as u64 { break; }
                if ms.cache.find(next as u64).is_some() { break; }
                run += 1;
            }
            let dst = unsafe {
                core::slice::from_raw_parts_mut(buf.add(done), run * BLOCK_SIZE)
            };
            if virtio_blk::read_blocks(ms.dev, phys as u64, dst) {
                done += run * BLOCK_SIZE;
                continue;
            }
            // Device error — fall through to the single-block cached path,
            // which reports the block via its own read (zeros on failure).
        }

        if phys == 0 {
            // Sparse block — return zeros
            unsafe { core::ptr::write_bytes(buf.add(done), 0, chunk); }
        } else {
            let dblk = ms.cache.read(ms.dev, phys as u64);
            unsafe { core::ptr::copy_nonoverlapping(dblk[blk_off..].as_ptr(), buf.add(done), chunk); }
        }
        done += chunk;
    }
    done
}

/// Write `count` bytes from `src` into file `ino` at `pos`.
fn write_file_data(ms: &mut MountState, ino: u32, pos: u64, src: *const u8, count: usize) -> usize {
    let iblkaddr = nat_lookup(ms, ino);
    let mut iblk_copy = {
        let b = ms.cache.read(ms.dev, iblkaddr as u64);
        let mut c = [0u8; BLOCK_SIZE]; c.copy_from_slice(b); c
    };

    let mut done = 0usize;
    while done < count {
        let file_off = pos + done as u64;
        let blk_idx  = file_off / BLOCK_SIZE as u64;
        let blk_off  = (file_off % BLOCK_SIZE as u64) as usize;
        let chunk    = (BLOCK_SIZE - blk_off).min(count - done);

        let phys = inode_logical_to_phys_for_write(ms, &mut iblk_copy, ino, iblkaddr, blk_idx);
        if phys == 0 { break; }

        if blk_off == 0 && chunk == BLOCK_SIZE {
            // Full block write
            let mut dbuf = [0u8; BLOCK_SIZE];
            unsafe { core::ptr::copy_nonoverlapping(src.add(done), dbuf.as_mut_ptr(), BLOCK_SIZE); }
            ms.cache.write(ms.dev, phys as u64, &dbuf);
        } else {
            // Partial block: read-modify-write
            let dblk = ms.cache.get_mut(ms.dev, phys as u64);
            unsafe { core::ptr::copy_nonoverlapping(src.add(done), dblk[blk_off..].as_mut_ptr(), chunk); }
        }
        done += chunk;
    }

    // Update file size
    let new_size = (pos + done as u64).max(inode_size(&iblk_copy));
    w64(&mut iblk_copy, INO_SIZE, new_size);
    ms.cache.write(ms.dev, iblkaddr as u64, &iblk_copy);
    nat_update(ms, ino, iblkaddr);

    maybe_flush(ms);
    done
}

/// Like inode_logical_to_phys but allocates a new block if missing.
fn inode_logical_to_phys_for_write(
    ms: &mut MountState,
    iblk: &mut [u8; BLOCK_SIZE],
    ino: u32,
    iblkaddr: u32,
    idx: u64,
) -> u32 {
    let max_direct = inode_max_direct(iblk) as u64;
    if idx < max_direct {
        let phys = inode_get_blkaddr(iblk, idx as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            inode_set_blkaddr(iblk, idx as usize, new_phys);
            ms.cache.write(ms.dev, iblkaddr as u64, iblk);
            return new_phys;
        }
        return 0;
    }

    let mut rem = idx - max_direct;
    const ADDRS_PER_DNODE: u64 = (NODE_FOOTER_OFF / 4) as u64; // 1019
    const NIDS_PER_BLOCK: u64 = (NODE_FOOTER_OFF / 4) as u64;  // 1019

    // Direct node 0: i_nid[0]
    if rem < ADDRS_PER_DNODE {
        let mut nid = inode_get_nid(iblk, 0);
        if nid == 0 {
            let (new_nid, _) = match create_node_block(ms, ino) {
                Some(v) => v,
                None => return 0,
            };
            nid = new_nid;
            inode_set_nid(iblk, 0, nid);
            ms.cache.write(ms.dev, iblkaddr as u64, iblk);
        }
        let dnblkaddr = nat_lookup(ms, nid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, rem as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            dnode_set_blkaddr(&mut dnblk_copy, rem as usize, new_phys);
            ms.cache.write(ms.dev, dnblkaddr as u64, &dnblk_copy);
            return new_phys;
        }
        return 0;
    }
    rem -= ADDRS_PER_DNODE;

    // Direct node 1: i_nid[1]
    if rem < ADDRS_PER_DNODE {
        let mut nid = inode_get_nid(iblk, 1);
        if nid == 0 {
            let (new_nid, _) = match create_node_block(ms, ino) {
                Some(v) => v,
                None => return 0,
            };
            nid = new_nid;
            inode_set_nid(iblk, 1, nid);
            ms.cache.write(ms.dev, iblkaddr as u64, iblk);
        }
        let dnblkaddr = nat_lookup(ms, nid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, rem as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            dnode_set_blkaddr(&mut dnblk_copy, rem as usize, new_phys);
            ms.cache.write(ms.dev, dnblkaddr as u64, &dnblk_copy);
            return new_phys;
        }
        return 0;
    }
    rem -= ADDRS_PER_DNODE;

    // Indirect node 0: i_nid[2]
    let blocks_per_indirect = NIDS_PER_BLOCK * ADDRS_PER_DNODE;
    if rem < blocks_per_indirect {
        let mut nid = inode_get_nid(iblk, 2);
        if nid == 0 {
            let (new_nid, _) = match create_node_block(ms, ino) {
                Some(v) => v,
                None => return 0,
            };
            nid = new_nid;
            inode_set_nid(iblk, 2, nid);
            ms.cache.write(ms.dev, iblkaddr as u64, iblk);
        }
        let ind_blkaddr = nat_lookup(ms, nid);
        let mut ind_blk = *ms.cache.read(ms.dev, ind_blkaddr as u64);
        let dnode_idx = rem / ADDRS_PER_DNODE;
        let dnode_off = rem % ADDRS_PER_DNODE;
        let mut dnid = dnode_get_blkaddr(&ind_blk, dnode_idx as usize);
        if dnid == 0 {
            let (new_nid, _) = match create_node_block(ms, ino) {
                Some(v) => v,
                None => return 0,
            };
            dnid = new_nid;
            dnode_set_blkaddr(&mut ind_blk, dnode_idx as usize, dnid);
            ms.cache.write(ms.dev, ind_blkaddr as u64, &ind_blk);
        }
        let dnblkaddr = nat_lookup(ms, dnid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, dnode_off as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            dnode_set_blkaddr(&mut dnblk_copy, dnode_off as usize, new_phys);
            ms.cache.write(ms.dev, dnblkaddr as u64, &dnblk_copy);
            return new_phys;
        }
        return 0;
    }
    rem -= blocks_per_indirect;

    // Indirect node 1: i_nid[3]
    if rem < blocks_per_indirect {
        let mut nid = inode_get_nid(iblk, 3);
        if nid == 0 {
            let (new_nid, _) = match create_node_block(ms, ino) {
                Some(v) => v,
                None => return 0,
            };
            nid = new_nid;
            inode_set_nid(iblk, 3, nid);
            ms.cache.write(ms.dev, iblkaddr as u64, iblk);
        }
        let ind_blkaddr = nat_lookup(ms, nid);
        let mut ind_blk = *ms.cache.read(ms.dev, ind_blkaddr as u64);
        let dnode_idx = rem / ADDRS_PER_DNODE;
        let dnode_off = rem % ADDRS_PER_DNODE;
        let mut dnid = dnode_get_blkaddr(&ind_blk, dnode_idx as usize);
        if dnid == 0 {
            let (new_nid, _) = match create_node_block(ms, ino) {
                Some(v) => v,
                None => return 0,
            };
            dnid = new_nid;
            dnode_set_blkaddr(&mut ind_blk, dnode_idx as usize, dnid);
            ms.cache.write(ms.dev, ind_blkaddr as u64, &ind_blk);
        }
        let dnblkaddr = nat_lookup(ms, dnid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, dnode_off as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            dnode_set_blkaddr(&mut dnblk_copy, dnode_off as usize, new_phys);
            ms.cache.write(ms.dev, dnblkaddr as u64, &dnblk_copy);
            return new_phys;
        }
        return 0;
    }
    rem -= blocks_per_indirect;

    // Double indirect node: i_nid[4]
    let blocks_per_dindirect = NIDS_PER_BLOCK * blocks_per_indirect;
    if rem < blocks_per_dindirect {
        let mut nid = inode_get_nid(iblk, 4);
        if nid == 0 {
            let (new_nid, _) = match create_node_block(ms, ino) {
                Some(v) => v,
                None => return 0,
            };
            nid = new_nid;
            inode_set_nid(iblk, 4, nid);
            ms.cache.write(ms.dev, iblkaddr as u64, iblk);
        }
        let dind_blkaddr = nat_lookup(ms, nid);
        let mut dind_blk = *ms.cache.read(ms.dev, dind_blkaddr as u64);
        let ind_idx = rem / blocks_per_indirect;
        let ind_rem = rem % blocks_per_indirect;
        let mut ind_nid = dnode_get_blkaddr(&dind_blk, ind_idx as usize);
        if ind_nid == 0 {
            let (new_nid, _) = match create_node_block(ms, ino) {
                Some(v) => v,
                None => return 0,
            };
            ind_nid = new_nid;
            dnode_set_blkaddr(&mut dind_blk, ind_idx as usize, ind_nid);
            ms.cache.write(ms.dev, dind_blkaddr as u64, &dind_blk);
        }
        let ind_blkaddr = nat_lookup(ms, ind_nid);
        let mut ind_blk = *ms.cache.read(ms.dev, ind_blkaddr as u64);
        let dnode_idx = ind_rem / ADDRS_PER_DNODE;
        let dnode_off = ind_rem % ADDRS_PER_DNODE;
        let mut dnid = dnode_get_blkaddr(&ind_blk, dnode_idx as usize);
        if dnid == 0 {
            let (new_nid, _) = match create_node_block(ms, ino) {
                Some(v) => v,
                None => return 0,
            };
            dnid = new_nid;
            dnode_set_blkaddr(&mut ind_blk, dnode_idx as usize, dnid);
            ms.cache.write(ms.dev, ind_blkaddr as u64, &ind_blk);
        }
        let dnblkaddr = nat_lookup(ms, dnid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, dnode_off as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            dnode_set_blkaddr(&mut dnblk_copy, dnode_off as usize, new_phys);
            ms.cache.write(ms.dev, dnblkaddr as u64, &dnblk_copy);
            return new_phys;
        }
        return 0;
    }

    0
}

/// Allocate a new direct-node block (for indirect addressing). Returns (nid, phys_blkaddr).
fn create_node_block(ms: &mut MountState, owner_ino: u32) -> Option<(u32, u32)> {
    let nid = ms.cp.next_free_nid;
    ms.cp.next_free_nid = nid.wrapping_add(1);
    let phys = alloc_node_block(ms)?;
    let mut buf = [0u8; BLOCK_SIZE];
    w32(&mut buf, NODE_FOOTER_OFF,     nid);
    w32(&mut buf, NODE_FOOTER_OFF + 4, owner_ino);
    ms.cache.write(ms.dev, phys as u64, &buf);
    nat_update(ms, nid, phys);
    Some((nid, phys))
}

// ── Directory operations ──────────────────────────────────────────────────────

/// Find `name` in directory `dir_ino`. Returns child inode or 0.
fn dir_lookup(ms: &mut MountState, dir_ino: u32, name: &[u8]) -> u32 {
    dir_lookup_ft(ms, dir_ino, name).0
}

/// `dir_lookup` that also reports the dentry's type byte.
///
/// The type byte is what lets path resolution decide whether a component is a
/// symlink *without* reading its inode: checking `i_mode` instead would add a
/// NAT lookup plus a block read to every component of every path, on a boot
/// path (execve of /bin/shell) that has no symlinks in it at all.
fn dir_lookup_ft(ms: &mut MountState, dir_ino: u32, name: &[u8]) -> (u32, u8) {
    let iblkaddr = nat_lookup(ms, dir_ino);
    let iblk_copy = {
        let b = ms.cache.read(ms.dev, iblkaddr as u64);
        let mut c = [0u8; BLOCK_SIZE]; c.copy_from_slice(b); c
    };
    let fsize = inode_size(&iblk_copy);
    let n_data_blks = fsize.div_ceil(BLOCK_SIZE as u64) as usize;

    for blk_idx in 0..n_data_blks {
        let phys = inode_logical_to_phys(ms, dir_ino, blk_idx as u64);
        if phys == 0 { continue; }
        let dblk = ms.cache.read(ms.dev, phys as u64);
        let dblk = unsafe { &*(dblk as *const [u8; BLOCK_SIZE]) };

        let mut slot = 0usize;
        while slot < NR_DENTRY_IN_BLK {
            let byte = slot / 8;
            let bit  = slot % 8;
            if byte < DENTRY_BITMAP_SIZE && (dblk[byte] & (1 << bit)) == 0 {
                slot += 1;
                continue;
            }
            let e_off = DENTRY_ENTRIES_OFF + slot * DENTRY_ENTRY_SIZE;
            if e_off + DENTRY_ENTRY_SIZE > BLOCK_SIZE { break; }
            let ino      = r32(dblk, e_off + 4);
            let name_len = r16(dblk, e_off + 8) as usize;
            if name_len == name.len() {
                let n_off = DENTRY_NAMES_OFF + slot * DENTRY_SLOT_LEN;
                if n_off + name_len <= BLOCK_SIZE
                    && &dblk[n_off..n_off + name_len] == name
                {
                    return (ino, dblk[e_off + 10]);
                }
            }
            let slots_used = (name_len + DENTRY_SLOT_LEN - 1) / DENTRY_SLOT_LEN;
            slot += slots_used.max(1);
        }
    }
    (0, 0)
}

/// Add a directory entry `(name, child_ino, file_type)` to `dir_ino`.
fn dir_add_entry(ms: &mut MountState, dir_ino: u32, name: &[u8], child_ino: u32, ftype: u8) -> bool {
    let slots_needed = (name.len() + DENTRY_SLOT_LEN - 1) / DENTRY_SLOT_LEN;
    let iblkaddr = nat_lookup(ms, dir_ino);
    let mut iblk_copy = {
        let b = ms.cache.read(ms.dev, iblkaddr as u64);
        let mut c = [0u8; BLOCK_SIZE]; c.copy_from_slice(b); c
    };
    let fsize = inode_size(&iblk_copy);
    let mut n_data_blks = fsize.div_ceil(BLOCK_SIZE as u64) as usize;

    // Try to find free slots in existing blocks first, then allocate new
    'outer: for blk_pass in 0..=n_data_blks {
        let phys = if blk_pass < n_data_blks {
            let p = inode_logical_to_phys(ms, dir_ino, blk_pass as u64);
            if p == 0 { continue; }
            p
        } else {
            // Allocate a new data block
            let p = match alloc_data_block(ms) { Some(p) => p, None => return false };
            // Zero it out
            let nb = [0u8; BLOCK_SIZE];
            ms.cache.write(ms.dev, p as u64, &nb);
            // Update inode
            inode_set_blkaddr(&mut iblk_copy, blk_pass, p);
            n_data_blks += 1;
            p
        };

        let dblk = ms.cache.get_mut(ms.dev, phys as u64);
        // Find `slots_needed` consecutive free slots
        let mut free_run = 0usize;
        let mut start_slot = 0usize;
        let mut scan = 0usize;
        while scan < NR_DENTRY_IN_BLK {
            let byte = scan / 8;
            let bit  = scan % 8;
            if byte >= DENTRY_BITMAP_SIZE { break; }
            if (dblk[byte] & (1 << bit)) == 0 {
                if free_run == 0 { start_slot = scan; }
                free_run += 1;
                if free_run >= slots_needed {
                    // Write entry at start_slot
                    let e_off = DENTRY_ENTRIES_OFF + start_slot * DENTRY_ENTRY_SIZE;
                    w32(dblk, e_off,     0); // hash (skip for MVP)
                    w32(dblk, e_off + 4, child_ino);
                    w16(dblk, e_off + 8, name.len() as u16);
                    dblk[e_off + 10] = ftype;
                    // Copy filename
                    let n_off = DENTRY_NAMES_OFF + start_slot * DENTRY_SLOT_LEN;
                    if n_off + name.len() <= BLOCK_SIZE {
                        dblk[n_off..n_off + name.len()].copy_from_slice(name);
                    }
                    // Set bitmap bits
                    for s in start_slot..start_slot + slots_needed {
                        let b = s / 8; let bit = s % 8;
                        if b < DENTRY_BITMAP_SIZE { dblk[b] |= 1 << bit; }
                    }
                    break 'outer;
                }
            } else {
                free_run = 0;
            }
            scan += 1;
        }
    }

    // Update inode size and write back
    let new_size = n_data_blks as u64 * BLOCK_SIZE as u64;
    w64(&mut iblk_copy, INO_SIZE, new_size);
    ms.cache.write(ms.dev, iblkaddr as u64, &iblk_copy);
    nat_update(ms, dir_ino, iblkaddr);
    maybe_flush(ms);
    true
}

/// Remove the directory entry for `name` from `dir_ino`. Returns true if found.
fn dir_remove_entry(ms: &mut MountState, dir_ino: u32, name: &[u8]) -> bool {
    let iblkaddr = nat_lookup(ms, dir_ino);
    let iblk_copy = {
        let b = ms.cache.read(ms.dev, iblkaddr as u64);
        let mut c = [0u8; BLOCK_SIZE]; c.copy_from_slice(b); c
    };
    let fsize = inode_size(&iblk_copy);
    let n_data_blks = fsize.div_ceil(BLOCK_SIZE as u64) as usize;

    for blk_idx in 0..n_data_blks {
        let phys = inode_logical_to_phys(ms, dir_ino, blk_idx as u64);
        if phys == 0 { continue; }
        let dblk = ms.cache.get_mut(ms.dev, phys as u64);
        let mut slot = 0usize;
        while slot < NR_DENTRY_IN_BLK {
            let byte = slot / 8;
            let bit  = slot % 8;
            if byte >= DENTRY_BITMAP_SIZE { break; }
            if (dblk[byte] & (1 << bit)) == 0 { slot += 1; continue; }
            let e_off    = DENTRY_ENTRIES_OFF + slot * DENTRY_ENTRY_SIZE;
            let name_len = r16(dblk, e_off + 8) as usize;
            let n_off    = DENTRY_NAMES_OFF + slot * DENTRY_SLOT_LEN;
            if name_len == name.len()
                && n_off + name_len <= BLOCK_SIZE
                && &dblk[n_off..n_off + name_len] == name
            {
                let slots_used = (name_len + DENTRY_SLOT_LEN - 1) / DENTRY_SLOT_LEN;
                for s in slot..slot + slots_used {
                    let b = s / 8; let bit = s % 8;
                    if b < DENTRY_BITMAP_SIZE { dblk[b] &= !(1 << bit); }
                }
                return true;
            }
            let slots_used = (name_len + DENTRY_SLOT_LEN - 1) / DENTRY_SLOT_LEN;
            slot += slots_used.max(1);
        }
    }
    false
}

// ── Path resolution ───────────────────────────────────────────────────────────

/// Rewrite a volume-relative path into `/`-rooted normal form, dropping empty
/// and "." components and resolving ".." lexically. Splicing a symlink body
/// back into a path reintroduces all three, so the walk re-normalises after
/// every hop.
fn normalize_volume_path(src: &[u8], out: &mut [u8; 256]) -> usize {
    let mut len = 1usize;
    out[0] = b'/';
    for comp in src.split(|&b| b == b'/') {
        if comp.is_empty() || comp == b"." { continue; }
        if comp == b".." {
            if len > 1 {
                let mut last = len - 1;
                while last > 0 && out[last] != b'/' { last -= 1; }
                len = if last == 0 { 1 } else { last };
            }
            continue;
        }
        if len > 1 { if len >= 255 { break; } out[len] = b'/'; len += 1; }
        let n = comp.len().min(255 - len);
        out[len..len + n].copy_from_slice(&comp[..n]);
        len += n;
    }
    len
}

/// Read a symlink inode's body (the target path) into `out`. Returns 0 for an
/// empty or unreadable link.
fn read_link_target(ms: &mut MountState, ino: u32, out: &mut [u8; 256]) -> usize {
    let addr = nat_lookup(ms, ino);
    if addr == 0 { return 0; }
    let size = { let b = ms.cache.read(ms.dev, addr as u64); inode_size(b) as usize };
    let n = size.min(255);
    if n == 0 { return 0; }
    read_file_data(ms, ino, 0, out.as_mut_ptr(), n)
}

/// Resolve an absolute path (with mount prefix stripped) to an inode.
/// Returns 0 on failure (not found, or ELOOP).
fn resolve_path(ms: &mut MountState, path: &[u8]) -> u32 {
    resolve_path_ex(ms, path, true)
}

/// Path walk with explicit control over the final component.
///
/// `follow_final == true` is what open/stat/chmod want; `false` is what
/// unlink/rename/readlink/lstat want — they act on the link, not its target.
/// Intermediate components are followed either way.
///
/// A relative target is spliced against the *symlink's own* parent directory
/// (`buf[..comp_start]`), never against anything caller-derived — resolving it
/// against the process cwd is the classic way to get this wrong.
///
/// LIMITATION: an absolute target is re-rooted at this volume's root. Since
/// this filesystem is mounted at "/" after pivot_root that is right for
/// everything the volume can reach, but a link into a namespace the VFS
/// intercepts ahead of the mount table (/tmp, /dev, /proc) resolves to 0 =
/// ENOENT rather than crossing over. Links the other way (a tmpfs symlink
/// naming an f2fs path) do work, because the VFS re-routes the resolved path.
fn resolve_path_ex(ms: &mut MountState, path: &[u8], follow_final: bool) -> u32 {
    let mut buf = [0u8; 256];
    let mut len = normalize_volume_path(path, &mut buf);
    let mut hops = 0u32;

    'restart: loop {
        if len <= 1 { return ms.sb.root_ino; }
        let mut ino = ms.sb.root_ino;
        let mut comp_start = 0usize; // index of the '/' before the component

        while comp_start < len {
            let mut comp_end = comp_start + 1;
            while comp_end < len && buf[comp_end] != b'/' { comp_end += 1; }

            // Copy the component out: dir_lookup needs &mut ms, which would
            // otherwise conflict with a borrow of `buf`.
            let mut name = [0u8; 256];
            let nlen = comp_end - comp_start - 1;
            name[..nlen].copy_from_slice(&buf[comp_start + 1..comp_end]);

            let (next, ftype) = dir_lookup_ft(ms, ino, &name[..nlen]);
            if next == 0 { return 0; }

            let is_last = comp_end == len;
            if ftype == DT_LNK && !(is_last && !follow_final) {
                hops += 1;
                if hops > SYMLINK_MAX_HOPS { return 0; } // ELOOP

                let mut target = [0u8; 256];
                let tlen = read_link_target(ms, next, &mut target);
                if tlen == 0 { return 0; }

                let mut spliced = [0u8; 640];
                let mut n = 0usize;
                let mut push = |bytes: &[u8], n: &mut usize| {
                    let take = bytes.len().min(640 - *n);
                    spliced[*n..*n + take].copy_from_slice(&bytes[..take]);
                    *n += take;
                };
                if target[0] == b'/' {
                    push(&target[..tlen], &mut n);
                } else {
                    push(&buf[..comp_start], &mut n);
                    push(b"/", &mut n);
                    push(&target[..tlen], &mut n);
                }
                push(&buf[comp_end..len], &mut n);
                drop(push);

                len = normalize_volume_path(&spliced[..n], &mut buf);
                continue 'restart;
            }

            ino = next;
            comp_start = comp_end;
        }
        return ino;
    }
}

/// Strip the mount prefix from a full path and return the remainder.
fn strip_prefix<'a>(path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if path.starts_with(prefix) {
        let rest = &path[prefix.len()..];
        if prefix == b"/" {
            Some(rest)
        } else if rest.is_empty() || rest[0] == b'/' {
            Some(rest)
        } else {
            None
        }
    } else {
        None
    }
}

fn get_relative_path<'a>(ms: &MountState, path: &'a [u8]) -> Option<&'a [u8]> {
    if let Some(r) = strip_prefix(path, ms.mount_prefix.as_bytes()) {
        return Some(r);
    }
    strip_prefix(path, b"/")
}

/// Return parent path and final component of path.
fn path_split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(pos) => (&path[..pos], &path[pos+1..]),
        None      => (b"", path),
    }
}

// ── VFS handler implementations ───────────────────────────────────────────────

fn handle_open(ms: &mut MountState, path_ptr: u64, flags: u64, _mode: u64) -> Message {
    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };

    let rel = match get_relative_path(ms, path_bytes) {
        Some(r) => r,
        None    => return err_reply(-2), // ENOENT
    };

    let writable = (flags & (O_WRONLY | O_RDWR)) != 0;
    let create   = (flags & O_CREAT) != 0;

    let ino = resolve_path(ms, rel);

    let ino = if ino == 0 {
        if !create || !writable { return err_reply(-2); } // ENOENT
        // Create the file
        let (parent_path, name) = path_split(rel);
        let parent_ino = if parent_path.is_empty() || parent_path == b"/" {
            ms.sb.root_ino
        } else {
            let p = resolve_path(ms, parent_path);
            if p == 0 { return err_reply(-2); }
            p
        };
        let mode = S_IFREG | 0o644;
        let (new_ino, _) = match create_inode(ms, mode, parent_ino, name) {
            Some(v) => v,
            None    => return err_reply(-28), // ENOSPC
        };
        if !dir_add_entry(ms, parent_ino, name, new_ino, DT_REG) {
            return err_reply(-28);
        }
        new_ino
    } else {
        // O_CREAT|O_EXCL means "fail if it already exists" — the atomic
        // lockfile idiom. Checked before O_TRUNC so a losing racer cannot
        // destroy the winner's file on its way to the error.
        if create && flags & O_EXCL != 0 { return err_reply(-17); } // EEXIST
        if flags & O_TRUNC != 0 {
            // Truncate: zero size in inode
            let iblkaddr = nat_lookup(ms, ino);
            let iblk = ms.cache.get_mut(ms.dev, iblkaddr as u64);
            w64(iblk, INO_SIZE, 0);
        }
        ino
    };

    // Find a free open-file slot
    let slot = match ms.open_files.iter().position(|f| !f.in_use) {
        Some(i) => i,
        None    => return err_reply(-24), // EMFILE
    };
    ms.open_files[slot] = OpenFile { inode: ino, pos: 0, writable, in_use: true,
                                     path: [0; MAX_OPEN_PATH], path_len: 0 };
    if path_bytes.len() <= MAX_OPEN_PATH {
        ms.open_files[slot].path[..path_bytes.len()].copy_from_slice(path_bytes);
        ms.open_files[slot].path_len = path_bytes.len();
    }
    val_reply(slot as u64)
}

fn handle_read(ms: &mut MountState, file_id: u64, buf_ptr: u64, count: u64) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); } // EBADF
    let ino = ms.open_files[slot].inode;
    let pos = ms.open_files[slot].pos;
    let n = read_file_data(ms, ino, pos, buf_ptr as *mut u8, count as usize);
    ms.open_files[slot].pos += n as u64;
    val_reply(n as u64)
}

fn handle_write(ms: &mut MountState, file_id: u64, buf_ptr: u64, count: u64) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); }
    if !ms.open_files[slot].writable { return err_reply(-13); } // EACCES
    let ino = ms.open_files[slot].inode;
    let pos = ms.open_files[slot].pos;
    let n = write_file_data(ms, ino, pos, buf_ptr as *const u8, count as usize);
    ms.open_files[slot].pos += n as u64;
    val_reply(n as u64)
}

fn handle_close(ms: &mut MountState, file_id: u64) -> Message {
    let slot = file_id as usize;
    if slot < MAX_OPEN_FILES { ms.open_files[slot].in_use = false; }
    ok_reply()
}

fn handle_lseek(ms: &mut MountState, file_id: u64, offset: u64, whence: u64) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); }
    let ino  = ms.open_files[slot].inode;
    let iblkaddr = nat_lookup(ms, ino);
    let iblk = ms.cache.read(ms.dev, iblkaddr as u64);
    let fsize = inode_size(iblk);
    let new_pos = match whence {
        0 => offset,
        1 => ms.open_files[slot].pos.wrapping_add(offset),
        2 => fsize.wrapping_add(offset),
        _ => return err_reply(-22),
    };
    ms.open_files[slot].pos = new_pos;
    val_reply(new_pos)
}

/// stat(2) — final component followed, so a symlink reports its target.
fn handle_stat(ms: &mut MountState, path_ptr: u64, stat_ptr: u64) -> Message {
    stat_common(ms, path_ptr, stat_ptr, true)
}

/// lstat(2) — final component NOT followed, so a symlink reports S_IFLNK and
/// the byte length of its target as st_size (which is what `ls -l` prints).
fn handle_lstat(ms: &mut MountState, path_ptr: u64, stat_ptr: u64) -> Message {
    stat_common(ms, path_ptr, stat_ptr, false)
}

fn stat_common(ms: &mut MountState, path_ptr: u64, stat_ptr: u64, follow: bool) -> Message {
    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let rel = match get_relative_path(ms, path_bytes) {
        Some(r) => r,
        None    => return err_reply(-2),
    };
    let ino = resolve_path(ms, rel);
    if ino == 0 { return err_reply(-2); }

    let iblkaddr = nat_lookup(ms, ino);
    let iblk = ms.cache.read(ms.dev, iblkaddr as u64);
    let mode  = inode_mode(iblk) as u32;
    let size  = inode_size(iblk);
    let links = inode_links(iblk) as u16;

    // Write a simple stat structure (struct stat layout from libc)
    // st_ino(8), st_mode(4), st_nlink(4), st_size(8) at known offsets
    let stat_buf = stat_ptr as *mut u8;
    unsafe {
        core::ptr::write_bytes(stat_buf, 0, 144); // zero the stat buf
        // st_ino at offset 8 (Linux x86-64 stat layout)
        core::ptr::copy_nonoverlapping((ino as u64).to_le_bytes().as_ptr(), stat_buf.add(8), 8);
        // st_mode at offset 24
        core::ptr::copy_nonoverlapping(mode.to_le_bytes().as_ptr(), stat_buf.add(24), 4);
        // st_nlink at offset 16 (as u64 in some layouts; use u32 here)
        core::ptr::copy_nonoverlapping((links as u32).to_le_bytes().as_ptr(), stat_buf.add(16), 4);
        // st_size at offset 48
        core::ptr::copy_nonoverlapping(size.to_le_bytes().as_ptr(), stat_buf.add(48), 8);
    }
    ok_reply()
}

fn handle_getdents(ms: &mut MountState, file_id: u64, buf_ptr: u64, count: u64) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); }
    let dir_ino = ms.open_files[slot].inode;
    let iblkaddr = nat_lookup(ms, dir_ino);
    let iblk_copy = {
        let b = ms.cache.read(ms.dev, iblkaddr as u64);
        let mut c = [0u8; BLOCK_SIZE]; c.copy_from_slice(b); c
    };
    if !inode_is_dir(&iblk_copy) { return err_reply(-20); } // ENOTDIR

    let fsize = inode_size(&iblk_copy);
    let n_data_blks = fsize.div_ceil(BLOCK_SIZE as u64) as usize;

    let mut written = 0usize;
    let max = count as usize;
    let current_pos = ms.open_files[slot].pos;
    let mut entry_idx = 0u64;

    // linux_dirent64 layout: ino(8)+off(8)+reclen(2)+type(1)+name(var)+null(1), padded to 8
    for blk_idx in 0..n_data_blks {
        let phys = inode_logical_to_phys(ms, dir_ino, blk_idx as u64);
        if phys == 0 { continue; }
        let dblk = ms.cache.read(ms.dev, phys as u64);
        let dblk = unsafe { &*(dblk as *const [u8; BLOCK_SIZE]) };

        let mut slot_idx = 0usize;
        while slot_idx < NR_DENTRY_IN_BLK {
            let byte = slot_idx / 8;
            let bit  = slot_idx % 8;
            if byte >= DENTRY_BITMAP_SIZE { break; }
            if (dblk[byte] & (1 << bit)) == 0 { slot_idx += 1; continue; }

            let e_off    = DENTRY_ENTRIES_OFF + slot_idx * DENTRY_ENTRY_SIZE;
            let child_ino = r32(dblk, e_off + 4) as u64;
            let name_len  = r16(dblk, e_off + 8) as usize;
            let ftype     = dblk[e_off + 10];
            let n_off     = DENTRY_NAMES_OFF + slot_idx * DENTRY_SLOT_LEN;

            // Compute dirent size: 8+8+2+1+name_len+1 rounded up to 8
            let base_size = 8 + 8 + 2 + 1 + name_len + 1;
            let reclen    = (base_size + 7) & !7;

            if entry_idx >= current_pos {
                if written + reclen > max {
                    ms.open_files[slot].pos = entry_idx;
                    return val_reply(written as u64);
                }

                unsafe {
                    let d = (buf_ptr as *mut u8).add(written);
                    // d_ino
                    core::ptr::copy_nonoverlapping(child_ino.to_le_bytes().as_ptr(), d, 8);
                    // d_off
                    let next_pos = entry_idx + 1;
                    core::ptr::copy_nonoverlapping(next_pos.to_le_bytes().as_ptr(), d.add(8), 8);
                    // d_reclen
                    core::ptr::copy_nonoverlapping((reclen as u16).to_le_bytes().as_ptr(), d.add(16), 2);
                    // d_type
                    *d.add(18) = ftype;
                    // d_name
                    if n_off + name_len <= BLOCK_SIZE {
                        core::ptr::copy_nonoverlapping(dblk[n_off..].as_ptr(), d.add(19), name_len);
                    }
                    *d.add(19 + name_len) = 0; // null terminator
                }
                written += reclen;
            }

            entry_idx += 1;
            let slots_used = (name_len + DENTRY_SLOT_LEN - 1) / DENTRY_SLOT_LEN;
            slot_idx += slots_used.max(1);
        }
    }
    ms.open_files[slot].pos = entry_idx;
    val_reply(written as u64)
}

fn handle_mkdir(ms: &mut MountState, path_ptr: u64, _mode: u64) -> Message {
    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let rel = match get_relative_path(ms, path_bytes) {
        Some(r) => r,
        None    => return err_reply(-2),
    };
    let (parent_rel, name) = path_split(rel);
    let parent_ino = if parent_rel.is_empty() || parent_rel == b"/" {
        ms.sb.root_ino
    } else {
        let p = resolve_path(ms, parent_rel);
        if p == 0 { return err_reply(-2); }
        p
    };
    // Check parent is a directory
    let parent_iblkaddr = nat_lookup(ms, parent_ino);
    {
        let iblk = ms.cache.read(ms.dev, parent_iblkaddr as u64);
        if !inode_is_dir(iblk) { return err_reply(-20); }
    }

    // Check name doesn't already exist
    if dir_lookup(ms, parent_ino, name) != 0 { return err_reply(-17); } // EEXIST

    let mode = S_IFDIR | 0o755;
    let (new_ino, _) = match create_inode(ms, mode, parent_ino, name) {
        Some(v) => v,
        None    => return err_reply(-28),
    };
    if !dir_add_entry(ms, parent_ino, name, new_ino, DT_DIR) {
        return err_reply(-28);
    }
    // Add "." and ".." entries to the new directory
    dir_add_entry(ms, new_ino, b".", new_ino, DT_DIR);
    dir_add_entry(ms, new_ino, b"..", parent_ino, DT_DIR);
    ok_reply()
}

fn handle_unlink(ms: &mut MountState, path_ptr: u64) -> Message {
    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let rel = match get_relative_path(ms, path_bytes) {
        Some(r) => r,
        None    => return err_reply(-2),
    };
    let (parent_rel, name) = path_split(rel);
    let parent_ino = if parent_rel.is_empty() || parent_rel == b"/" {
        ms.sb.root_ino
    } else {
        resolve_path(ms, parent_rel)
    };
    if parent_ino == 0 { return err_reply(-2); }
    let ino = dir_lookup(ms, parent_ino, name);
    if ino == 0 { return err_reply(-2); }
    let iblkaddr = nat_lookup(ms, ino);
    let is_dir = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_is_dir(iblk) };
    if is_dir { return err_reply(-21); } // EISDIR — use rmdir() instead
    if !dir_remove_entry(ms, parent_ino, name) { return err_reply(-2); }

    // Drop one reference. The inode block and its data blocks are only
    // reclaimable once the count reaches zero — and this server has never
    // reclaimed them (create/delete leaks blocks until the next mkfs), so the
    // count is what stops a *surviving* hard link from being treated as the
    // last name. Without it, `ln a b && rm a` left `b` pointing at an inode
    // whose i_links_count still read 2, which every fsck and every st_nlink
    // consumer would then disbelieve.
    let links = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_links(iblk) };
    if links > 1 {
        let iblk = ms.cache.get_mut(ms.dev, iblkaddr as u64);
        w32(iblk, INO_LINKS, links - 1);
        nat_update(ms, ino, iblkaddr);
    }
    maybe_flush(ms);
    ok_reply()
}

/// symlink(target, linkpath) — create a symlink inode holding `target` as its
/// file data.
///
/// The volume is built with `^inline_data`, so even a two-byte target costs a
/// full data block. That matches how every other file on this volume is
/// stored and keeps the read path (`read_file_data`) the single one.
fn handle_symlink(ms: &mut MountState, target_ptr: u64, link_ptr: u64) -> Message {
    let target = unsafe {
        let ptr = target_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 && len < 255 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let link_bytes = unsafe {
        let ptr = link_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    if target.is_empty() { return err_reply(-2); } // ENOENT

    let rel = match get_relative_path(ms, link_bytes) {
        Some(r) => r,
        None    => return err_reply(-2),
    };
    let (parent_rel, name) = path_split(rel);
    if name.is_empty() { return err_reply(-17); } // EEXIST — the mount point
    let parent_ino = if parent_rel.is_empty() || parent_rel == b"/" {
        ms.sb.root_ino
    } else {
        resolve_path(ms, parent_rel)
    };
    if parent_ino == 0 { return err_reply(-2); }
    if dir_lookup(ms, parent_ino, name) != 0 { return err_reply(-17); } // EEXIST

    // Copy the target off the caller's buffer before any allocation runs: the
    // pointer is only guaranteed live for the duration of this call, and the
    // block writes below can flush and re-enter the cache.
    let mut tbuf = [0u8; 256];
    let tlen = target.len().min(255);
    tbuf[..tlen].copy_from_slice(&target[..tlen]);

    let (new_ino, _) = match create_inode(ms, S_IFLNK | 0o777, parent_ino, name) {
        Some(v) => v,
        None    => return err_reply(-28), // ENOSPC
    };
    if write_file_data(ms, new_ino, 0, tbuf.as_ptr(), tlen) != tlen {
        return err_reply(-28); // ENOSPC
    }
    if !dir_add_entry(ms, parent_ino, name, new_ino, DT_LNK) {
        return err_reply(-28);
    }
    maybe_flush(ms);
    ok_reply()
}

/// readlink(path, buf, len) — EINVAL when the path exists but is not a link.
/// `VFS_FD_PATH(slot, buf, len)` — the absolute path an open fd was opened by,
/// which is how `readlink("/proc/self/fd/N")` is answered for a file on this
/// mount. Without it the VFS had no answer for `VnodeKind::MountedFile` at all
/// and returned EBADF, so the standard "recover a filename from an fd" idiom
/// failed for every file outside RamFS and tmpfs.
fn handle_fd_path(ms: &mut MountState, slot: u64, buf_ptr: u64, buf_len: u64) -> Message {
    if buf_ptr == 0 || buf_len == 0 { return err_reply(-14); }
    let slot = slot as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); } // EBADF
    let n = ms.open_files[slot].path_len;
    if n == 0 { return err_reply(-2); } // ENOENT — path was too long to record
    let n = n.min(buf_len as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(ms.open_files[slot].path.as_ptr(), buf_ptr as *mut u8, n);
    }
    val_reply(n as u64)
}

fn handle_readlink(ms: &mut MountState, path_ptr: u64, buf_ptr: u64, buf_len: u64) -> Message {
    if buf_ptr == 0 || buf_len == 0 { return err_reply(-14); }
    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let rel = match get_relative_path(ms, path_bytes) {
        Some(r) => r,
        None    => return err_reply(-2),
    };
    let ino = resolve_path_ex(ms, rel, false);
    if ino == 0 { return err_reply(-2); }

    let addr = nat_lookup(ms, ino);
    let (mode, size) = {
        let iblk = ms.cache.read(ms.dev, addr as u64);
        (inode_mode(iblk), inode_size(iblk))
    };
    if mode & S_IFMT != S_IFLNK { return err_reply(-22); } // EINVAL

    let n = (size as usize).min(buf_len as usize);
    if n == 0 { return val_reply(0); }
    let got = read_file_data(ms, ino, 0, buf_ptr as *mut u8, n);
    val_reply(got as u64)
}

/// link(oldpath, newpath) — a second dentry pointing at the same nid, with
/// i_links_count bumped. Exactly the shape scripts/mkfs-f2fs-populated.py
/// already writes for the ~105 coreutils names that share one inode, so the
/// read path needs no changes at all.
fn handle_link(ms: &mut MountState, old_ptr: u64, new_ptr: u64) -> Message {
    let read_path = |p: u64| unsafe {
        let ptr = p as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let old_bytes = read_path(old_ptr);
    let new_bytes = read_path(new_ptr);

    let old_rel = match get_relative_path(ms, old_bytes) { Some(r) => r, None => return err_reply(-2) };
    let new_rel = match get_relative_path(ms, new_bytes) { Some(r) => r, None => return err_reply(-18) }; // EXDEV

    // link(2) does not follow the final component of either path.
    let src_ino = resolve_path_ex(ms, old_rel, false);
    if src_ino == 0 { return err_reply(-2); }

    let addr = nat_lookup(ms, src_ino);
    let (mode, links) = {
        let iblk = ms.cache.read(ms.dev, addr as u64);
        (inode_mode(iblk), inode_links(iblk))
    };
    // Hard links to directories are EPERM: a directory cycle has no safe
    // unwind, so only the filesystem's own "." / ".." may point at one.
    if mode & S_IFMT == S_IFDIR { return err_reply(-1); } // EPERM

    let (parent_rel, name) = path_split(new_rel);
    if name.is_empty() { return err_reply(-17); }
    let parent_ino = if parent_rel.is_empty() || parent_rel == b"/" {
        ms.sb.root_ino
    } else {
        resolve_path(ms, parent_rel)
    };
    if parent_ino == 0 { return err_reply(-2); }
    if dir_lookup(ms, parent_ino, name) != 0 { return err_reply(-17); } // EEXIST

    let ftype = match mode & S_IFMT {
        S_IFLNK => DT_LNK,
        _       => DT_REG,
    };
    if !dir_add_entry(ms, parent_ino, name, src_ino, ftype) { return err_reply(-28); }

    let iblk = ms.cache.get_mut(ms.dev, addr as u64);
    w32(iblk, INO_LINKS, links + 1);
    nat_update(ms, src_ino, addr);
    maybe_flush(ms);
    ok_reply()
}

/// chmod(2) via path — follows the final symlink component, same group as
/// VFS_OPEN/VFS_STAT in the VFS's `path_args()` table.
fn handle_chmod(ms: &mut MountState, path_ptr: u64, mode: u32) -> Message {
    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let rel = match get_relative_path(ms, path_bytes) {
        Some(r) => r,
        None    => return err_reply(-2), // ENOENT
    };
    let ino = resolve_path(ms, rel);
    if ino == 0 { return err_reply(-2); }
    chmod_inode(ms, ino, mode)
}

/// fchmod(2) — the fd is already resolved to an inode via the open-file
/// table, so no path walk (and no symlink-follow question) is involved.
fn handle_fchmod(ms: &mut MountState, file_id: u64, mode: u32) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); } // EBADF
    let ino = ms.open_files[slot].inode;
    chmod_inode(ms, ino, mode)
}

/// Mutate i_mode in place and write the inode block back, mirroring the
/// link-count update in handle_unlink/handle_link (nat_update + maybe_flush,
/// no separate checksum: this simplified node-block footer carries no
/// checksum field to fix up).
///
/// Only the permission/setuid/setgid/sticky bits change — the file-type
/// bits (S_IFMT) are exactly what create_inode wrote and chmod(2) must
/// never touch them.
fn chmod_inode(ms: &mut MountState, ino: u32, mode: u32) -> Message {
    let addr = nat_lookup(ms, ino);
    let iblk = ms.cache.get_mut(ms.dev, addr as u64);
    let cur = inode_mode(iblk);
    let new_mode = (cur & S_IFMT) | (mode as u16 & !S_IFMT);
    w16(iblk, INO_MODE, new_mode);
    nat_update(ms, ino, addr);
    maybe_flush(ms);
    ok_reply()
}

/// chown(2) via path. `u32::MAX` for `uid`/`gid` means "leave unchanged" —
/// mirrors chown(2)'s `-1` and matches apply_chown's tmpfs handling in
/// servers/vfs/src/lib.rs.
///
/// NOTE: like VFS_CHMOD, the VFS always follows the final symlink component
/// for VFS_CHOWN (see path_args()), so this cannot honor AT_SYMLINK_NOFOLLOW
/// (lchown) — there is no VFS_LCHOWN tag, and upstream of this server
/// kernel/src/syscall.rs's sys_fchownat() already discards its `flags`
/// argument before a distinction could even reach the VFS. Fixing that is a
/// kernel + VFS protocol change, out of this server's scope.
fn handle_chown(ms: &mut MountState, path_ptr: u64, uid: u32, gid: u32) -> Message {
    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let rel = match get_relative_path(ms, path_bytes) {
        Some(r) => r,
        None    => return err_reply(-2),
    };
    let ino = resolve_path(ms, rel);
    if ino == 0 { return err_reply(-2); }
    chown_inode(ms, ino, uid, gid)
}

/// fchown(2) — fd already resolved to an inode via the open-file table.
fn handle_fchown(ms: &mut MountState, file_id: u64, uid: u32, gid: u32) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); }
    let ino = ms.open_files[slot].inode;
    chown_inode(ms, ino, uid, gid)
}

/// Mutate i_uid/i_gid in place and write the inode block back, same
/// nat_update + maybe_flush shape as chmod_inode/handle_link.
fn chown_inode(ms: &mut MountState, ino: u32, uid: u32, gid: u32) -> Message {
    let addr = nat_lookup(ms, ino);
    let iblk = ms.cache.get_mut(ms.dev, addr as u64);
    if uid != u32::MAX { w32(iblk, INO_UID, uid); }
    if gid != u32::MAX { w32(iblk, INO_GID, gid); }
    nat_update(ms, ino, addr);
    maybe_flush(ms);
    ok_reply()
}

/// True if directory `dir_ino` contains no entries other than "." and "..".
fn dir_is_empty(ms: &mut MountState, dir_ino: u32) -> bool {
    let iblkaddr = nat_lookup(ms, dir_ino);
    let iblk_copy = {
        let b = ms.cache.read(ms.dev, iblkaddr as u64);
        let mut c = [0u8; BLOCK_SIZE]; c.copy_from_slice(b); c
    };
    let fsize = inode_size(&iblk_copy);
    let n_data_blks = fsize.div_ceil(BLOCK_SIZE as u64) as usize;

    for blk_idx in 0..n_data_blks {
        let phys = inode_logical_to_phys(ms, dir_ino, blk_idx as u64);
        if phys == 0 { continue; }
        let dblk = ms.cache.read(ms.dev, phys as u64);
        let dblk = unsafe { &*(dblk as *const [u8; BLOCK_SIZE]) };
        let mut slot = 0usize;
        while slot < NR_DENTRY_IN_BLK {
            let byte = slot / 8;
            let bit  = slot % 8;
            if byte >= DENTRY_BITMAP_SIZE { break; }
            if (dblk[byte] & (1 << bit)) == 0 { slot += 1; continue; }
            let e_off    = DENTRY_ENTRIES_OFF + slot * DENTRY_ENTRY_SIZE;
            let name_len = r16(dblk, e_off + 8) as usize;
            let n_off    = DENTRY_NAMES_OFF + slot * DENTRY_SLOT_LEN;
            let is_dot    = name_len == 1 && n_off + 1 <= BLOCK_SIZE && dblk[n_off] == b'.';
            let is_dotdot = name_len == 2 && n_off + 2 <= BLOCK_SIZE && &dblk[n_off..n_off + 2] == b"..";
            if !is_dot && !is_dotdot { return false; }
            let slots_used = (name_len + DENTRY_SLOT_LEN - 1) / DENTRY_SLOT_LEN;
            slot += slots_used.max(1);
        }
    }
    true
}

fn handle_rmdir(ms: &mut MountState, path_ptr: u64) -> Message {
    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let rel = match get_relative_path(ms, path_bytes) {
        Some(r) => r,
        None    => return err_reply(-2),
    };
    let (parent_rel, name) = path_split(rel);
    let parent_ino = if parent_rel.is_empty() || parent_rel == b"/" {
        ms.sb.root_ino
    } else {
        resolve_path(ms, parent_rel)
    };
    if parent_ino == 0 { return err_reply(-2); }
    let ino = dir_lookup(ms, parent_ino, name);
    if ino == 0 { return err_reply(-2); }
    let iblkaddr = nat_lookup(ms, ino);
    let is_dir = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_is_dir(iblk) };
    if !is_dir { return err_reply(-20); } // ENOTDIR
    if !dir_is_empty(ms, ino) { return err_reply(-39); } // ENOTEMPTY
    if dir_remove_entry(ms, parent_ino, name) { maybe_flush(ms); ok_reply() }
    else { err_reply(-2) }
}

fn handle_rename(ms: &mut MountState, old_ptr: u64, new_ptr: u64) -> Message {
    let old_bytes = unsafe {
        let ptr = old_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let new_bytes = unsafe {
        let ptr = new_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let old_rel = match get_relative_path(ms, old_bytes) {
        Some(r) => r, None => return err_reply(-2),
    };
    let new_rel = match get_relative_path(ms, new_bytes) {
        Some(r) => r, None => return err_reply(-2),
    };
    let (old_parent_rel, old_name) = path_split(old_rel);
    let (new_parent_rel, new_name) = path_split(new_rel);
    let old_parent_ino = if old_parent_rel.is_empty() || old_parent_rel == b"/" {
        ms.sb.root_ino
    } else {
        resolve_path(ms, old_parent_rel)
    };
    let new_parent_ino = if new_parent_rel.is_empty() || new_parent_rel == b"/" {
        ms.sb.root_ino
    } else {
        resolve_path(ms, new_parent_rel)
    };
    if old_parent_ino == 0 || new_parent_ino == 0 { return err_reply(-2); }
    let ino = dir_lookup(ms, old_parent_ino, old_name);
    if ino == 0 { return err_reply(-2); }
    if dir_lookup(ms, new_parent_ino, new_name) != 0 { return err_reply(-17); } // EEXIST
    let iblkaddr = nat_lookup(ms, ino);
    let is_dir = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_is_dir(iblk) };
    let ftype = if is_dir { DT_DIR } else { DT_REG };
    if !dir_add_entry(ms, new_parent_ino, new_name, ino, ftype) { return err_reply(-28); } // ENOSPC
    dir_remove_entry(ms, old_parent_ino, old_name);
    if is_dir && new_parent_ino != old_parent_ino {
        dir_remove_entry(ms, ino, b"..");
        dir_add_entry(ms, ino, b"..", new_parent_ino, DT_DIR);
    }
    maybe_flush(ms);
    ok_reply()
}

fn handle_ftruncate(ms: &mut MountState, file_id: u64, length: u64) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); }
    if !ms.open_files[slot].writable { return err_reply(-13); }
    let ino = ms.open_files[slot].inode;
    let iblkaddr = nat_lookup(ms, ino);
    let iblk = ms.cache.get_mut(ms.dev, iblkaddr as u64);
    w64(iblk, INO_SIZE, length);
    maybe_flush(ms);
    ok_reply()
}

// ── IPC dispatch ──────────────────────────────────────────────────────────────

fn f2fs_dispatch(msg: &Message, _caller_pid: u32, target_port: u32) -> Message {
    let mut mounts = F2FS_MOUNTS.lock();
    for slot in mounts.iter_mut() {
        if let Some(ref mut ms) = slot {
            if ms.port == target_port {
                return dispatch_msg(ms, msg);
            }
        }
    }
    err_reply(-5) // EIO — no mount found for this port
}

fn dispatch_msg(ms: &mut MountState, msg: &Message) -> Message {
    match msg.tag {
        VFS_OPEN       => handle_open(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_READ       => handle_read(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_WRITE      => handle_write(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_CLOSE      => handle_close(ms, arg(msg,0)),
        VFS_LSEEK      => handle_lseek(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_STAT       => handle_stat(ms, arg(msg,0), arg(msg,1)),
        VFS_GETDENTS64 => handle_getdents(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_MKDIR      => handle_mkdir(ms, arg(msg,0), arg(msg,1)),
        VFS_UNLINK     => handle_unlink(ms, arg(msg,0)),
        VFS_RMDIR      => handle_rmdir(ms, arg(msg,0)),
        VFS_RENAME     => handle_rename(ms, arg(msg,0), arg(msg,1)),
        VFS_FTRUNCATE  => handle_ftruncate(ms, arg(msg,0), arg(msg,1)),
        VFS_STATFS     => handle_statfs(ms, arg(msg,1)),
        VFS_LSTAT      => handle_lstat(ms, arg(msg,0), arg(msg,1)),
        VFS_SYMLINK    => handle_symlink(ms, arg(msg,0), arg(msg,1)),
        VFS_FD_PATH    => handle_fd_path(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_READLINK   => handle_readlink(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_LINK       => handle_link(ms, arg(msg,0), arg(msg,1)),
        VFS_CHMOD      => handle_chmod(ms, arg(msg,0), arg(msg,1) as u32),
        VFS_FCHMOD     => handle_fchmod(ms, arg(msg,0), arg(msg,1) as u32),
        VFS_CHOWN      => handle_chown(ms, arg(msg,0), arg(msg,1) as u32, arg(msg,2) as u32),
        VFS_FCHOWN     => handle_fchown(ms, arg(msg,0), arg(msg,1) as u32, arg(msg,2) as u32),
        _              => err_reply(-22), // EINVAL
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// `/dev/vd<letter>` names for device indices 0..7, used to populate the
/// live mount table (`servers/vfs`) with something `/proc/mounts`/`lsblk`
/// can display — mirrors the naming `sys_mount` already parses in reverse.
const DEV_NAMES: [&str; 8] = [
    "/dev/vda", "/dev/vdb", "/dev/vdc", "/dev/vdd",
    "/dev/vde", "/dev/vdf", "/dev/vdg", "/dev/vdh",
];

/// Mount the F2FS volume on block device `dev_idx` at `mount_point`.
/// Returns the IPC port of the server, or None on failure.
pub fn mount(dev_idx: usize, mount_point: &'static str, owner_pid: u32) -> Option<u32> {
    // Read and parse superblock
    let mut block0 = [0u8; BLOCK_SIZE];
    if !virtio_blk::read_block(dev_idx, 0, &mut block0) { return None; }
    let sb = SbInfo::parse(&block0)?;

    // Read both checkpoint packs; select the one with higher version
    let pack_blks = (sb.seg_cnt_ckpt / 2) * sb.blocks_per_seg;
    let mut cp_buf0 = [0u8; BLOCK_SIZE];
    let mut cp_buf1 = [0u8; BLOCK_SIZE];
    virtio_blk::read_block(dev_idx, sb.cp_blkaddr as u64, &mut cp_buf0);
    virtio_blk::read_block(dev_idx, (sb.cp_blkaddr + pack_blks) as u64, &mut cp_buf1);

    let (ver0, mut cp0) = CpInfo::parse_pack(&cp_buf0);
    let (ver1, mut cp1) = CpInfo::parse_pack(&cp_buf1);
    cp0.active_pack = 0;
    cp1.active_pack = 1;
    let cp = if ver0 >= ver1 { cp0 } else { cp1 };

    // Allocate a slot in the global mount table
    let mut mounts = F2FS_MOUNTS.lock();
    let slot_idx = mounts.iter().position(|s| s.is_none())?;

    let port = port::create(owner_pid)?;
    port::register_handler(port, f2fs_dispatch);

    mounts[slot_idx] = Some(MountState {
        dev:          dev_idx,
        port,
        mount_prefix: mount_point,
        sb,
        cp,
        dirty_writes: 0,
        open_files:   [const { OpenFile::empty() }; MAX_OPEN_FILES],
        cache:        BlockCache::new(),
    });

    drop(mounts);
    let device_str = DEV_NAMES.get(dev_idx).copied().unwrap_or("/dev/vd?");
    vfs_server::register_mount(mount_point, port, device_str, "f2fs");

    Some(port)
}

/// Positional read for the kernel's demand-paged exec path.
///
/// Reads `len` bytes at byte `pos` of the open file `file_id` on the mount
/// whose IPC port is `port`, into `dst` (a kernel HHDM pointer).  Unlike
/// VFS_READ this never touches the open file's seek position, so concurrent
/// page faults on the same backing file cannot race on it.  Called directly
/// (no IPC) because it runs from page-fault context; everything below is
/// synchronous polling I/O.
///
/// Returns bytes read, or a negative errno.
pub fn pread_by_port(port: u32, file_id: u64, dst: *mut u8, len: usize, pos: u64) -> isize {
    let mut mounts = F2FS_MOUNTS.lock();
    for slot in mounts.iter_mut() {
        if let Some(ref mut ms) = slot {
            if ms.port == port {
                let idx = file_id as usize;
                if idx >= MAX_OPEN_FILES || !ms.open_files[idx].in_use {
                    return -9; // EBADF
                }
                let ino = ms.open_files[idx].inode;
                return read_file_data(ms, ino, pos, dst, len) as isize;
            }
        }
    }
    -9 // EBADF — no mount on this port
}

/// Close an open-file slot on the mount whose IPC port is `port` — the
/// direct-call twin of VFS_CLOSE, used when the kernel releases the backing
/// file of a demand-paged exec image.
pub fn close_by_port(port: u32, file_id: u64) {
    let mut mounts = F2FS_MOUNTS.lock();
    for slot in mounts.iter_mut() {
        if let Some(ref mut ms) = slot {
            if ms.port == port {
                let idx = file_id as usize;
                if idx < MAX_OPEN_FILES {
                    ms.open_files[idx].in_use = false;
                }
                return;
            }
        }
    }
}

/// Unmount the F2FS volume mounted at `mount_point`.
/// Returns true on success, false if no volume was mounted at that point.
pub fn unmount(mount_point: &str) -> bool {
    let mut mounts = F2FS_MOUNTS.lock();
    if let Some(slot_idx) = mounts.iter().position(|s| s.as_ref().map_or(false, |m| m.mount_prefix == mount_point)) {
        if let Some(mut ms) = mounts[slot_idx].take() {
            // Flush cache and checkpoint
            flush_checkpoint(&mut ms);
            // Close IPC port (implicitly unregisters handler)
            port::close(ms.port);
            // Unregister from VFS
            vfs_server::unregister_mount(mount_point);
            return true;
        }
    }
    false
}

