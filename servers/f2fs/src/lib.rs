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
const VFS_FSTAT:      u64 = 0x31;
const VFS_READLINK:   u64 = 0x36;
const VFS_LINK:       u64 = 0x37;
const VFS_LSTAT:      u64 = 0x38;
const VFS_CHMOD:      u64 = 0x2B;
const VFS_FCHMOD:     u64 = 0x2C;
const VFS_CHOWN:      u64 = 0x2D;
const VFS_FCHOWN:     u64 = 0x2E;
// These MUST stay in lockstep with servers/vfs/src/lib.rs. They are duplicated
// rather than imported, and an *undefined* upper-case name in the dispatch
// `match` is not an error — Rust reads it as a catch-all binding that silently
// swallows every later arm. That is exactly how VFS_LCHMOD/LCHOWN, added
// without these definitions, made VFS_CHOWN unreachable and chown a no-op.
const VFS_FSYNC:      u64 = 0x39;
const VFS_LCHMOD:     u64 = 0x3B;
const VFS_LCHOWN:     u64 = 0x3C;
// Extended-attribute / POSIX-ACL ops. Same footgun applies: every one of these
// names is used in the dispatch `match` below, so an *undefined* upper-case name
// there would become a catch-all binding and silently swallow the arms after it.
// All 13 are defined here, in lockstep with servers/vfs/src/lib.rs.
const VFS_SETXATTR:     u64 = 0x3D;
const VFS_LSETXATTR:    u64 = 0x3E;
const VFS_FSETXATTR:    u64 = 0x3F;
const VFS_GETXATTR:     u64 = 0x40;
const VFS_LGETXATTR:    u64 = 0x41;
const VFS_FGETXATTR:    u64 = 0x42;
const VFS_LISTXATTR:    u64 = 0x43;
const VFS_LLISTXATTR:   u64 = 0x44;
const VFS_FLISTXATTR:   u64 = 0x45;
const VFS_REMOVEXATTR:  u64 = 0x46;
const VFS_LREMOVEXATTR: u64 = 0x47;
const VFS_FREMOVEXATTR: u64 = 0x48;
const VFS_ACCESS:       u64 = 0x49;

const O_WRONLY:  u64 = 1;
const O_RDWR:    u64 = 2;
const O_CREAT:   u64 = 0o100;
const O_EXCL:    u64 = 0o200;
const O_TRUNC:   u64 = 0o1000;
/// Refuse to open a symlink through its target (`ELOOP`). Security-relevant:
/// it is how a privileged writer avoids being redirected by a symlink an
/// unprivileged user planted in a shared directory.
const O_NOFOLLOW:  u64 = 0o400000;
/// Fail with `ENOTDIR` unless the result is a directory.
const O_DIRECTORY: u64 = 0o200000;

// ── F2FS on-disk byte-offset constants ────────────────────────────────────────

const BLOCK_SIZE: usize = 4096;
const F2FS_MAGIC: u32 = 0xF2F5_2010;
const F2FS_SB_OFFSET: usize = 1024; // within first block

// Superblock offsets (relative to F2FS_SB_OFFSET within block 0)
const SB_MAGIC:            usize = 0;
const SB_LOG_BLK_PER_SEG:  usize = 20;
/// `__le64 block_count` — total 4 KiB blocks in the volume, including the
/// metadata areas. Offset 36 in `struct f2fs_super_block`.
const SB_BLOCK_COUNT:      usize = 36;
const SB_SEG_CNT_CKPT:     usize = 52;
const SB_SEG_CNT_NAT:       usize = 60;
/// `__le32 segment_count_main` — segments in the main (user data) area. This
/// times `blocks_per_seg` is Linux's `sbi->user_block_count`.
const SB_SEG_CNT_MAIN:      usize = 68;
/// `__le32 segment0_blkaddr` — first block of segment 0; blocks below it are
/// not part of any segment, and Linux subtracts it from `block_count` to get
/// `f_blocks`.
const SB_SEGMENT0_BLKADDR:  usize = 72;
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
// `__le32 i_xattr_nid` — the nid of this inode's dedicated xattr node block, or
// 0 when it has no extended attributes. Bytes 24..84 are verified never written
// by scripts/mkfs-f2fs-populated.py (i_size ends at 24, i_pino starts at 84),
// so every existing on-disk inode reads back 0 here = "no xattrs", and mkfs
// needs no change. See the xattr node-block layout in the setxattr path.
const INO_XATTR:     usize = 24;
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
    block_count: u64,
    seg_cnt_ckpt: u32,
    seg_cnt_nat: u32,
    seg_cnt_main: u32,
    segment0_blkaddr: u32,
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
            block_count:     r64(sb, SB_BLOCK_COUNT),
            seg_cnt_ckpt:    r32(sb, SB_SEG_CNT_CKPT),
            seg_cnt_nat:     r32(sb, SB_SEG_CNT_NAT),
            seg_cnt_main:    r32(sb, SB_SEG_CNT_MAIN),
            segment0_blkaddr: r32(sb, SB_SEGMENT0_BLKADDR),
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
    // Bound the scan by the real main-area segment count, not a hardcoded
    // 1024: on a volume with fewer segments the modulo wrapped onto segments
    // past the end of SIT, and on a larger one it left everything above 1024
    // permanently unreachable.
    let total = ms.sb.seg_cnt_main.max(1);
    let start_seg = (after + 1) % total;
    for seg_off in 0..total {
        let seg = (start_seg + seg_off) % total;
        // Never hand back a segment one of the logs is *currently* writing
        // into. Before reclaim existed this could not happen — vblocks only
        // ever grew — but once a segment can drop back to zero valid blocks
        // while the log's write pointer still sits inside it, allocating it to
        // the other log would overwrite live data. This is the one guard that
        // makes freeing safe; it is not optional.
        if seg == ms.cp.cur_data_segno || seg == ms.cp.cur_node_segno { continue; }
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
    let old_cnt = v & SIT_VBLOCKS_MASK;
    let cnt = old_cnt + 1;
    let new_v = (v & !SIT_VBLOCKS_MASK) | (cnt & SIT_VBLOCKS_MASK);
    w16(blk, entry_off, new_v);
    // Set bit in valid_map
    let byte_idx = blk_in_seg as usize / 8;
    let bit      = blk_in_seg as usize % 8;
    if entry_off + SIT_VMAP_OFF + byte_idx < BLOCK_SIZE {
        blk[entry_off + SIT_VMAP_OFF + byte_idx] |= 1 << bit;
    }
    let _ = old_cnt;
    // NOTE: free_seg_cnt (which backs statfs/df) is deliberately NOT maintained
    // here. It cannot be made accurate on this volume: the on-disk SIT vblocks
    // counts are not reliably maintained by mkfs or the existing write path
    // (a fresh write can leave a data segment reading a lower vblocks than it
    // holds), so neither an incremental counter nor a live SIT scan yields a
    // trustworthy free count. df therefore keeps reporting the static mkfs
    // value, exactly as it did before reclaim existed. Making it truthful needs
    // the allocator/SIT accounting reworked first — out of scope here.
}

/// Split a physical block address into `(segment, block-within-segment)`, or
/// `None` for the hole sentinel / anything below the main area.
///
/// Every block-tree getter returns 0 for an unallocated slot, so the `phys == 0`
/// guard is load-bearing: without it a sparse file's holes would each try to
/// "free" segment 0, block 0 — the start of the main area.
fn blkaddr_to_seg(ms: &MountState, phys: u32) -> Option<(u32, u32)> {
    if phys < ms.sb.main_blkaddr { return None; }
    let off = phys - ms.sb.main_blkaddr;
    let bps = ms.sb.blocks_per_seg;
    Some((off / bps, off % bps))
}

/// Clear one block's valid bit and decrement its segment's count — the inverse
/// of `sit_mark_block_used`.
///
/// Idempotent: the count is only decremented if the bit was actually set. A
/// double free otherwise corrupts vblocks, and with the reclaim walk touching
/// shared indirect structures a block *can* be reached twice, so this is not
/// theoretical.
fn sit_mark_block_free(ms: &mut MountState, seg: u32, blk_in_seg: u32) {
    let sit_blk_idx = seg / SIT_PER_BLK as u32;
    let sit_entry   = (seg % SIT_PER_BLK as u32) as usize;
    let sit_blkno   = ms.sb.sit_blkaddr + sit_blk_idx;
    let blk = ms.cache.get_mut(ms.dev, sit_blkno as u64);
    let entry_off = sit_entry * SIT_ENTRY_SIZE;
    let byte_idx = blk_in_seg as usize / 8;
    let bit      = blk_in_seg as usize % 8;
    let map_off  = entry_off + SIT_VMAP_OFF + byte_idx;
    if map_off >= BLOCK_SIZE { return; }
    let was_set = blk[map_off] & (1 << bit) != 0;
    if !was_set { return; }
    blk[map_off] &= !(1 << bit);
    let v = r16(blk, entry_off);
    let cnt = (v & SIT_VBLOCKS_MASK).saturating_sub(1);
    w16(blk, entry_off, (v & !SIT_VBLOCKS_MASK) | cnt);
    // See sit_mark_block_used on why free_seg_cnt is not touched. What matters
    // for reclaim is that the valid_map bit is cleared and vblocks decremented,
    // so sit_find_free_seg will hand this block's segment back once it empties.
}

/// Release a single physical block back to the allocator. No-op for holes.
fn free_block(ms: &mut MountState, phys: u32) {
    if let Some((seg, blk)) = blkaddr_to_seg(ms, phys) {
        sit_mark_block_free(ms, seg, blk);
    }
}

/// Sum the valid-block counts across every main-area segment, reading each SIT
/// block *through the cache* so counts dirtied by an alloc or a reclaim that
/// has not been checkpointed yet are still seen. `statfs` reports free space as
/// `user_blocks - this`, so it is current the instant a block is used or freed.
///
/// This replaces `cp.free_seg_cnt` — a whole-segment counter parsed once at
/// mount and never adjusted by the allocator or by reclaim — which is why df
/// stayed frozen at the mkfs value across create/delete/reclaim churn. The
/// vblocks field, by contrast, is maintained on every `sit_mark_block_used` /
/// `sit_mark_block_free`, so a live sum of it tracks reality.
fn sit_count_valid_blocks(ms: &mut MountState) -> u64 {
    let total = ms.sb.seg_cnt_main;
    let mut valid = 0u64;
    for seg in 0..total {
        let sit_blk_idx = seg / SIT_PER_BLK as u32;
        let sit_entry   = (seg % SIT_PER_BLK as u32) as usize;
        let sit_blkno   = ms.sb.sit_blkaddr + sit_blk_idx;
        let sit_blk = ms.cache.read(ms.dev, sit_blkno as u64);
        let entry_off = sit_entry * SIT_ENTRY_SIZE;
        valid += (r16(sit_blk, entry_off) & SIT_VBLOCKS_MASK) as u64;
    }
    valid
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
    // The checkpoint block is the commit record for everything flushed above
    // it, so it is the one write that must actually be on the medium before
    // this function claims the volume is consistent.
    virtio_blk::flush(ms.dev);

    ms.dirty_writes = 0;
}

/// fsync(fd) — the file's data and metadata must be on stable storage when
/// this returns.
///
/// This server has no per-file dirty tracking: the block cache is shared
/// across the volume and the checkpoint is what makes any of it recoverable.
/// So an fsync of one file is necessarily a checkpoint of the whole volume —
/// heavier than Linux's, but honest, which the previous unconditional `0`
/// was not.
fn handle_fsync(ms: &mut MountState) -> Message {
    flush_checkpoint(ms);
    ok_reply()
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
fn create_inode(ms: &mut MountState, mode: u16, uid: u32, gid: u32,
                parent_ino: u32, name: &[u8]) -> Option<(u32, u32)> {
    let ino = ms.cp.next_free_nid;
    ms.cp.next_free_nid = ino.wrapping_add(1);

    let phys = alloc_node_block(ms)?;
    let mut buf = [0u8; BLOCK_SIZE];

    w16(&mut buf, INO_MODE,    mode);
    // Ownership has to be recorded at creation. Leaving these zero made every
    // file on the volume claim root, which is not merely cosmetic: an
    // ownership check against a uid nothing ever sets can never deny anything.
    w32(&mut buf, INO_UID,     uid);
    w32(&mut buf, INO_GID,     gid);
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

/// Copy a block out of the cache onto the stack.
///
/// Every reclaim walk must do this before it frees anything: `free_block`
/// takes `cache.get_mut` on a SIT block, and with only `CACHE_SLOTS` slots
/// that can evict the very node block being walked. Holding a `&` into the
/// cache across a free is a use-after-evict.
fn read_block_copy(ms: &mut MountState, blkaddr: u32) -> [u8; BLOCK_SIZE] {
    let b = ms.cache.read(ms.dev, blkaddr as u64);
    let mut c = [0u8; BLOCK_SIZE];
    c.copy_from_slice(b);
    c
}

/// Free every data block reachable through direct node `nid` (1019 slots),
/// then the direct-node block itself.
fn free_dnode(ms: &mut MountState, nid: u32) {
    if nid == 0 { return; }
    let dblkaddr = nat_lookup(ms, nid);
    if dblkaddr == 0 { return; }
    let dblk = read_block_copy(ms, dblkaddr);
    const ADDRS_PER_DNODE: usize = NODE_FOOTER_OFF / 4; // 1019
    for i in 0..ADDRS_PER_DNODE {
        free_block(ms, dnode_get_blkaddr(&dblk, i));
    }
    free_block(ms, dblkaddr);
}

/// Free every block owned by inode `ino` — all data blocks and the entire node
/// tree (direct, indirect, double-indirect), but not the inode block itself
/// (the caller frees that, since only it knows whether the inode survives).
///
/// This is the whole-file case (unlink, rmdir, truncate-to-zero). It does not
/// bother zeroing the freed pointers because every caller is about to discard
/// or re-initialise the inode. NAT entries and nids are deliberately *not*
/// recycled here — see the module notes on why nid reuse is unsafe without a
/// free list.
fn free_inode_data_and_nodes(ms: &mut MountState, ino: u32) {
    let iblkaddr = nat_lookup(ms, ino);
    if iblkaddr == 0 { return; }
    let iblk = read_block_copy(ms, iblkaddr);

    // Inline direct addresses.
    let max_direct = inode_max_direct(&iblk);
    for i in 0..max_direct {
        free_block(ms, inode_get_blkaddr(&iblk, i));
    }

    // i_nid[0], i_nid[1]: direct nodes.
    free_dnode(ms, inode_get_nid(&iblk, 0));
    free_dnode(ms, inode_get_nid(&iblk, 1));

    const NIDS_PER_BLOCK: usize = NODE_FOOTER_OFF / 4; // 1019

    // i_nid[2], i_nid[3]: single-indirect — a block of dnode nids.
    for slot in 2..=3usize {
        let ind_nid = inode_get_nid(&iblk, slot);
        if ind_nid == 0 { continue; }
        let ind_blkaddr = nat_lookup(ms, ind_nid);
        if ind_blkaddr == 0 { continue; }
        let ind_blk = read_block_copy(ms, ind_blkaddr);
        for i in 0..NIDS_PER_BLOCK {
            free_dnode(ms, dnode_get_blkaddr(&ind_blk, i));
        }
        free_block(ms, ind_blkaddr);
    }

    // i_nid[4]: double-indirect — a block of indirect-node nids.
    let dind_nid = inode_get_nid(&iblk, 4);
    if dind_nid != 0 {
        let dind_blkaddr = nat_lookup(ms, dind_nid);
        if dind_blkaddr != 0 {
            let dind_blk = read_block_copy(ms, dind_blkaddr);
            for i in 0..NIDS_PER_BLOCK {
                let ind_nid = dnode_get_blkaddr(&dind_blk, i);
                if ind_nid == 0 { continue; }
                let ind_blkaddr = nat_lookup(ms, ind_nid);
                if ind_blkaddr == 0 { continue; }
                let ind_blk = read_block_copy(ms, ind_blkaddr);
                for j in 0..NIDS_PER_BLOCK {
                    free_dnode(ms, dnode_get_blkaddr(&ind_blk, j));
                }
                free_block(ms, ind_blkaddr);
            }
            free_block(ms, dind_blkaddr);
        }
    }

    // The dedicated xattr node block, freed the same way as an indirect node:
    // resolve its nid through the NAT and hand the physical block back to the
    // allocator. Like every other nid here its NAT entry is left stale (nids are
    // never recycled without a free list), and the inode block itself — which
    // still carries INO_XATTR — is discarded by the caller.
    let xnid = r32(&iblk, INO_XATTR);
    if xnid != 0 {
        let xaddr = nat_lookup(ms, xnid);
        if xaddr != 0 { free_block(ms, xaddr); }
    }
}

/// Free every block of `ino` and reset all of its block pointers to zero,
/// leaving a valid empty file. Used for truncate-to-zero (and O_TRUNC); it is
/// just the whole-file case of `truncate_to`.
fn truncate_to_zero(ms: &mut MountState, ino: u32) {
    truncate_to(ms, ino, 0);
}

/// Shrink `ino` to `new_len` bytes: free every data block wholly past the new
/// end, free any node block that empties out, zero the sub-block tail so a
/// later extension reads zeros there, and write the new `i_size`.
///
/// Structural walk (mirrors `free_inode_data_and_nodes`), *not* the
/// per-logical-index `inode_logical_to_phys` walk — that one returned scattered
/// addresses past the inline-direct region and mis-freed live blocks. Each
/// level copies its node block to the stack before touching `free_block`, which
/// takes the SIT block through the 4-slot cache and can evict the node being
/// walked; the freed data slots (logical index `>= keep`) are cleared in the
/// stack copy, and a node with nothing live left is itself freed and unlinked
/// from its parent. NAT entries for freed nids are deliberately left stale: nid
/// reuse is unsafe without a free list, exactly as in the unlink path.
fn truncate_to(ms: &mut MountState, ino: u32, new_len: u64) {
    let iblkaddr = nat_lookup(ms, ino);
    if iblkaddr == 0 { return; }

    // Number of leading data blocks to keep. `keep == 0` frees everything —
    // the truncate-to-zero case.
    let keep = (new_len + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;

    // Zero from the new EOF to the end of its (kept) block, so a later
    // extension reads zeros rather than the stale bytes left in that block.
    let tail_off = (new_len % BLOCK_SIZE as u64) as usize;
    if tail_off != 0 {
        let phys = inode_logical_to_phys(ms, ino, new_len / BLOCK_SIZE as u64);
        if phys != 0 {
            let dblk = ms.cache.get_mut(ms.dev, phys as u64);
            for b in &mut dblk[tail_off..] { *b = 0; }
        }
    }

    const ADDRS: u64 = (NODE_FOOTER_OFF / 4) as u64; // 1019
    const NIDS:  u64 = (NODE_FOOTER_OFF / 4) as u64; // 1019

    let mut iblk = read_block_copy(ms, iblkaddr);

    // Inline direct addresses: logical index == slot.
    let max_direct = inode_max_direct(&iblk);
    for i in 0..max_direct {
        if i as u64 >= keep {
            free_block(ms, inode_get_blkaddr(&iblk, i));
            inode_set_blkaddr(&mut iblk, i, 0);
        }
    }
    let mut base = max_direct as u64;

    // i_nid[0], i_nid[1]: direct nodes.
    for slot in 0..=1usize {
        let nid = inode_get_nid(&iblk, slot);
        if truncate_dnode(ms, nid, base, keep) { inode_set_nid(&mut iblk, slot, 0); }
        base += ADDRS;
    }

    // i_nid[2], i_nid[3]: single-indirect.
    let per_ind = NIDS * ADDRS;
    for slot in 2..=3usize {
        let nid = inode_get_nid(&iblk, slot);
        if truncate_indirect(ms, nid, base, keep) { inode_set_nid(&mut iblk, slot, 0); }
        base += per_ind;
    }

    // i_nid[4]: double-indirect.
    let dind_nid = inode_get_nid(&iblk, 4);
    if truncate_dindirect(ms, dind_nid, base, keep) { inode_set_nid(&mut iblk, 4, 0); }

    w64(&mut iblk, INO_SIZE, new_len);
    ms.cache.write(ms.dev, iblkaddr as u64, &iblk);
    nat_update(ms, ino, iblkaddr);
}

/// Free the data blocks of direct node `nid` whose logical index is `>= keep`
/// (its slots cover indices `[base, base + 1019)`). Returns `true` when the
/// node block itself was freed because nothing live remained, so the caller
/// must clear its pointer.
fn truncate_dnode(ms: &mut MountState, nid: u32, base: u64, keep: u64) -> bool {
    const ADDRS_PER_DNODE: usize = NODE_FOOTER_OFF / 4; // 1019
    if nid == 0 { return false; }
    // Wholly within the kept region: nothing to do, and skipping avoids reading
    // (and re-dirtying) every node block of a large file on a small shrink.
    if base + ADDRS_PER_DNODE as u64 <= keep { return false; }
    let dblkaddr = nat_lookup(ms, nid);
    if dblkaddr == 0 { return false; }
    let mut dblk = read_block_copy(ms, dblkaddr);
    let mut any_kept = false;
    for i in 0..ADDRS_PER_DNODE {
        if base + i as u64 >= keep {
            free_block(ms, dnode_get_blkaddr(&dblk, i));
            dnode_set_blkaddr(&mut dblk, i, 0);
        } else if dnode_get_blkaddr(&dblk, i) != 0 {
            any_kept = true;
        }
    }
    if any_kept {
        ms.cache.write(ms.dev, dblkaddr as u64, &dblk);
        false
    } else {
        free_block(ms, dblkaddr);
        true
    }
}

/// One level up from `truncate_dnode`: an indirect node holding up to 1019
/// direct-node nids, each covering 1019 logical indices.
fn truncate_indirect(ms: &mut MountState, nid: u32, base: u64, keep: u64) -> bool {
    const NIDS_PER_BLOCK: usize = NODE_FOOTER_OFF / 4; // 1019
    const ADDRS: u64 = (NODE_FOOTER_OFF / 4) as u64;
    if nid == 0 { return false; }
    if base + NIDS_PER_BLOCK as u64 * ADDRS <= keep { return false; }
    let ind_blkaddr = nat_lookup(ms, nid);
    if ind_blkaddr == 0 { return false; }
    let mut ind_blk = read_block_copy(ms, ind_blkaddr);
    let mut any_kept = false;
    for i in 0..NIDS_PER_BLOCK {
        let child_nid = dnode_get_blkaddr(&ind_blk, i);
        if child_nid == 0 { continue; }
        let child_base = base + i as u64 * ADDRS;
        if truncate_dnode(ms, child_nid, child_base, keep) {
            dnode_set_blkaddr(&mut ind_blk, i, 0);
        } else {
            any_kept = true;
        }
    }
    if any_kept {
        ms.cache.write(ms.dev, ind_blkaddr as u64, &ind_blk);
        false
    } else {
        free_block(ms, ind_blkaddr);
        true
    }
}

/// One more level up: a double-indirect node holding up to 1019 indirect-node
/// nids.
fn truncate_dindirect(ms: &mut MountState, nid: u32, base: u64, keep: u64) -> bool {
    const NIDS_PER_BLOCK: usize = NODE_FOOTER_OFF / 4; // 1019
    const ADDRS: u64 = (NODE_FOOTER_OFF / 4) as u64;
    let per_ind = NIDS_PER_BLOCK as u64 * ADDRS;
    if nid == 0 { return false; }
    if base + NIDS_PER_BLOCK as u64 * per_ind <= keep { return false; }
    let dind_blkaddr = nat_lookup(ms, nid);
    if dind_blkaddr == 0 { return false; }
    let mut dind_blk = read_block_copy(ms, dind_blkaddr);
    let mut any_kept = false;
    for i in 0..NIDS_PER_BLOCK {
        let child_nid = dnode_get_blkaddr(&dind_blk, i);
        if child_nid == 0 { continue; }
        let child_base = base + i as u64 * per_ind;
        if truncate_indirect(ms, child_nid, child_base, keep) {
            dnode_set_blkaddr(&mut dind_blk, i, 0);
        } else {
            any_kept = true;
        }
    }
    if any_kept {
        ms.cache.write(ms.dev, dind_blkaddr as u64, &dind_blk);
        false
    } else {
        free_block(ms, dind_blkaddr);
        true
    }
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
/// Lexically normalise a volume-relative path. `floor` is the byte offset below
/// which `..` may not climb — 1 (the volume root) normally, or the length of a
/// chroot jail's volume-relative root when confining a jailed symlink, so that
/// an absolute link target cannot use `..` to escape the jail.
fn normalize_volume_path_floor(src: &[u8], out: &mut [u8; 256], floor: usize) -> usize {
    let floor = floor.max(1);
    let mut len = 1usize;
    out[0] = b'/';
    for comp in src.split(|&b| b == b'/') {
        if comp.is_empty() || comp == b"." { continue; }
        if comp == b".." {
            if len > floor {
                let mut last = len - 1;
                while last > 0 && out[last] != b'/' { last -= 1; }
                let clamped = if last == 0 { 1 } else { last };
                len = if clamped < floor { floor } else { clamped };
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

fn normalize_volume_path(src: &[u8], out: &mut [u8; 256]) -> usize {
    normalize_volume_path_floor(src, out, 1)
}

/// The calling task's chroot root expressed in this volume's coordinates, or an
/// empty slice when the task is not chrooted or its jail lies on another mount.
///
/// f2fs resolves paths in volume-relative space (the mount prefix is already
/// stripped), so to confine a jailed symlink we need the jail root in the same
/// space. Runs in the caller's context (synchronous IPC), so `sched::current_root`
/// names the right task without any protocol change.
fn caller_jail_rel(ms: &MountState, out: &mut [u8; 128]) -> usize {
    let mut host = [0u8; 256];
    let n = sched::current_root(host.as_mut_ptr(), 256);
    if n <= 1 { return 0; }
    let n = (n as usize).min(255);
    match get_relative_path(ms, &host[..n]) {
        Some(rel) if rel.len() > 1 => {
            let take = rel.len().min(128);
            out[..take].copy_from_slice(&rel[..take]);
            take
        }
        _ => 0, // jail is "/" of this volume, or on another mount → no prefix
    }
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
    // Jail root in this volume's coordinates (empty = not confined here). An
    // absolute symlink target must re-anchor here, not at the volume root, or
    // a link inside a jail that sits below the mount point can climb out.
    let mut jail = [0u8; 128];
    let jlen = caller_jail_rel(ms, &mut jail);
    let floor = if jlen > 1 { jlen } else { 1 };

    let mut buf = [0u8; 256];
    let mut len = normalize_volume_path_floor(path, &mut buf, floor);
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
                    // Absolute target: re-anchor at the jail root so it cannot
                    // reach volume paths above the jail. Unjailed, jlen == 0 and
                    // this is the old verbatim behaviour.
                    if jlen > 1 { push(&jail[..jlen], &mut n); }
                    push(&target[..tlen], &mut n);
                } else {
                    push(&buf[..comp_start], &mut n);
                    push(b"/", &mut n);
                    push(&target[..tlen], &mut n);
                }
                push(&buf[comp_end..len], &mut n);
                drop(push);

                len = normalize_volume_path_floor(&spliced[..n], &mut buf, floor);
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

fn handle_open(ms: &mut MountState, path_ptr: u64, flags: u64, mode: u64,
               euid: u32, egid: u32) -> Message {
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
    let nofollow = (flags & O_NOFOLLOW) != 0;

    // With O_NOFOLLOW the final component must not be traversed, so that a
    // symlink resolves to *itself* and can then be rejected below. Resolving
    // first and checking afterwards would already have opened the target.
    let ino = if nofollow {
        resolve_path_ex(ms, rel, false)
    } else {
        resolve_path(ms, rel)
    };

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
        // The caller's mode, not a hardcoded 0644. umask is applied kernel-side
        // (where Linux applies it) so tmpfs and f2fs cannot disagree about it.
        let imode = S_IFREG | (mode as u16 & 0o7777);
        let (new_ino, _) = match create_inode(ms, imode, euid, egid, parent_ino, name) {
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
        // Permission gate for opening an *existing* object. handle_open used to
        // grant access purely on path resolution — mode bits were recorded but
        // never enforced, so any file was openable for read or write regardless
        // of its permissions. Mirror what the VFS does for tmpfs
        // (`check_access` in servers/vfs): derive want_read/want_write from the
        // access mode and consult the inode via `xattr::access_check`, which
        // honours a stored POSIX ACL. Root (euid 0) bypasses, so the boot path
        // (init/getty/login all run as root before setuid) is unaffected; the
        // freshly-created branch above is not gated (the creator owns it). The
        // check precedes O_TRUNC so an unwritable file is never truncated.
        if euid != 0 {
            let want_read  = flags & O_WRONLY == 0; // RDONLY/RDWR read; WRONLY does not
            let want_write = writable;
            let (meta, xnid) = load_meta_xnid(ms, ino);
            let xbuf = if xnid != 0 { Some(read_xattr_arena(ms, xnid)) } else { None };
            let acl = match &xbuf {
                Some(b) => xattr::find(&b[..xattr::F2FS_XATTR_ARENA], xattr::IDX_ACL_ACCESS, b""),
                None => None,
            };
            if !xattr::access_check(&meta, euid, egid, acl, want_read, want_write, false) {
                return err_reply(-13); // EACCES
            }
        }
        if flags & O_TRUNC != 0 {
            // Truncate to zero: free the data blocks, not just the size field.
            // Opening an existing file O_WRONLY|O_TRUNC is the usual way a
            // shell rewrites it (`> file`), so leaking here leaked on every
            // overwrite.
            let iblkaddr = nat_lookup(ms, ino);
            let old_size = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_size(iblk) };
            if old_size > 0 {
                truncate_to_zero(ms, ino);
            }
            let iblk = ms.cache.get_mut(ms.dev, iblkaddr as u64);
            w64(iblk, INO_SIZE, 0);
            nat_update(ms, ino, iblkaddr);
        }
        ino
    };

    // Reject the two type-conditional flags now that the target is known.
    // A freshly created file is always a regular file, so neither can fire on
    // the create path — but checking unconditionally keeps the rule in one
    // place rather than duplicated into the `else` arm above.
    {
        let mode = {
            let iblkaddr = nat_lookup(ms, ino);
            let iblk = ms.cache.read(ms.dev, iblkaddr as u64);
            inode_mode(iblk)
        };
        if nofollow && (mode & S_IFMT) == S_IFLNK { return err_reply(-40); } // ELOOP
        if flags & O_DIRECTORY != 0 && (mode & S_IFMT) != S_IFDIR {
            return err_reply(-20); // ENOTDIR
        }
    }

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

/// `VFS_FSTAT(file_id, stat_ptr)` — stat an open fd by its slot rather than by
/// path. The VFS cannot answer this itself for a mounted file: only the inode
/// records the real type, size and owner. Without it the VFS reported every
/// mounted fd as a plain `S_IFREG`, so `fstat` on a *directory* fd — which
/// musl's `fdopendir` issues before every `readdir` — came back "regular file"
/// and every fd-based directory walk (`rm -r`, `du`, GNU fts) got ENOTDIR on a
/// real directory. Fills the buffer exactly like `stat_common`, only starting
/// from the fd's recorded inode instead of a resolved path.
fn handle_fstat(ms: &mut MountState, file_id: u64, stat_ptr: u64) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); } // EBADF
    let ino = ms.open_files[slot].inode;
    let iblkaddr = nat_lookup(ms, ino);
    let iblk = ms.cache.read(ms.dev, iblkaddr as u64);
    let mode  = inode_mode(iblk) as u32;
    let size  = inode_size(iblk);
    let links = inode_links(iblk);
    let uid   = inode_uid(iblk);
    let gid   = inode_gid(iblk);
    vfs_server::write_stat_full(stat_ptr as usize, mode, links as u64, size, ino as u64, uid, gid);
    ok_reply()
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
    let ino = resolve_path_ex(ms, rel, follow);
    if ino == 0 { return err_reply(-2); }

    let iblkaddr = nat_lookup(ms, ino);
    let iblk = ms.cache.read(ms.dev, iblkaddr as u64);
    let mode  = inode_mode(iblk) as u32;
    let size  = inode_size(iblk);
    let links = inode_links(iblk);
    let uid   = inode_uid(iblk);
    let gid   = inode_gid(iblk);

    // Emit the stat struct in the target's native layout. This used to
    // open-code the x86-64 offsets, which put st_mode and st_nlink in the
    // wrong slots on AArch64 (the two fields swap places there) and wrote
    // 144 bytes into a 128-byte buffer. `mode` here is the on-disk i_mode,
    // so it already carries the real type + permission bits (0o100755 for
    // the /bin binaries), and `links` the real hard-link count. `uid`/`gid`
    // used to be hardcoded 0/0 since nothing ever persisted a chown — now
    // that handle_chown writes INO_UID/INO_GID, stat reflects it.
    vfs_server::write_stat_full(stat_ptr as usize, mode, links as u64, size, ino as u64, uid, gid);
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

fn handle_mkdir(ms: &mut MountState, path_ptr: u64, mode: u64,
                euid: u32, egid: u32) -> Message {
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

    let imode = S_IFDIR | (mode as u16 & 0o7777);
    let (new_ino, _) = match create_inode(ms, imode, euid, egid, parent_ino, name) {
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

    // Drop one reference. Blocks are reclaimable only when the count reaches
    // zero, so the count is also what stops a *surviving* hard link from being
    // treated as the last name: without it, `ln a b && rm a` left `b` pointing
    // at an inode whose i_links_count still read 2, which every fsck and every
    // st_nlink consumer would then disbelieve.
    let links = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_links(iblk) };
    if links > 1 {
        let iblk = ms.cache.get_mut(ms.dev, iblkaddr as u64);
        w32(iblk, INO_LINKS, links - 1);
        nat_update(ms, ino, iblkaddr);
    } else if ino_is_open(ms, ino) {
        // Last link, but a descriptor still holds it open. Linux keeps the
        // inode alive until the final close; this server has no per-inode
        // refcount, so the safe approximation is to leak the blocks rather
        // than free storage a live fd is still reading. Zero the link count so
        // the name is gone and a future fsck can reclaim it.
        let iblk = ms.cache.get_mut(ms.dev, iblkaddr as u64);
        w32(iblk, INO_LINKS, 0);
        nat_update(ms, ino, iblkaddr);
    } else {
        // Last link, not open: reclaim for real. Data blocks and the whole
        // node tree first, then the inode block itself.
        free_inode_data_and_nodes(ms, ino);
        free_block(ms, iblkaddr);
    }
    maybe_flush(ms);
    ok_reply()
}

/// True if any open descriptor on this mount still names `ino`.
///
/// Reclaim must skip an inode that is open — see the unlink path. Cheap linear
/// scan; `MAX_OPEN_FILES` is small.
fn ino_is_open(ms: &MountState, ino: u32) -> bool {
    ms.open_files.iter().any(|f| f.in_use && f.inode == ino)
}

/// symlink(target, linkpath) — create a symlink inode holding `target` as its
/// file data.
///
/// The volume is built with `^inline_data`, so even a two-byte target costs a
/// full data block. That matches how every other file on this volume is
/// stored and keeps the read path (`read_file_data`) the single one.
fn handle_symlink(ms: &mut MountState, target_ptr: u64, link_ptr: u64,
                  euid: u32, egid: u32) -> Message {
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

    let (new_ino, _) = match create_inode(ms, S_IFLNK | 0o777, euid, egid, parent_ino, name) {
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
/// `follow` is false for the AT_SYMLINK_NOFOLLOW form (VFS_LCHMOD), where the
/// caller means the symlink itself rather than what it points at.
fn handle_chmod(ms: &mut MountState, path_ptr: u64, mode: u32, follow: bool,
                euid: u32) -> Message {
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
    let ino = resolve_path_ex(ms, rel, follow);
    if ino == 0 { return err_reply(-2); }
    chmod_inode(ms, ino, mode, euid)
}

/// fchmod(2) — the fd is already resolved to an inode via the open-file
/// table, so no path walk (and no symlink-follow question) is involved.
fn handle_fchmod(ms: &mut MountState, file_id: u64, mode: u32, euid: u32) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); } // EBADF
    let ino = ms.open_files[slot].inode;
    chmod_inode(ms, ino, mode, euid)
}

/// Mutate i_mode in place and write the inode block back, mirroring the
/// link-count update in handle_unlink/handle_link (nat_update + maybe_flush,
/// no separate checksum: this simplified node-block footer carries no
/// checksum field to fix up).
///
/// Only the permission/setuid/setgid/sticky bits change — the file-type
/// bits (S_IFMT) are exactly what create_inode wrote and chmod(2) must
/// never touch them.
/// `euid` is the caller's effective uid. Only the owner or root may change a
/// file's mode; this mirrors what tmpfs already enforced in `apply_chown`
/// (servers/vfs), so the two filesystems agree.
fn chmod_inode(ms: &mut MountState, ino: u32, mode: u32, euid: u32) -> Message {
    let addr = nat_lookup(ms, ino);
    {
        let iblk = ms.cache.read(ms.dev, addr as u64);
        let owner = inode_uid(iblk);
        if euid != 0 && euid != owner { return err_reply(-1); } // EPERM
    }
    let new_mode;
    {
        let iblk = ms.cache.get_mut(ms.dev, addr as u64);
        let cur = inode_mode(iblk);
        new_mode = (cur & S_IFMT) | (mode as u16 & !S_IFMT);
        w16(iblk, INO_MODE, new_mode);
    }
    nat_update(ms, ino, addr);

    // Keep a stored access ACL consistent with the new mode (posix_acl_chmod):
    // USER_OBJ←owner, OTHER←other, MASK-or-GROUP_OBJ←group. Named entries are
    // untouched. Trivial ACLs are never stored, so this only fires when a real
    // ACL exists, and the write-back mirrors the setxattr read-modify-write.
    let xnid = { let iblk = ms.cache.read(ms.dev, addr as u64); r32(iblk, INO_XATTR) };
    if xnid != 0 {
        let mut blkbuf = read_xattr_arena(ms, xnid);
        let mut vbuf = [0u8; xattr::F2FS_XATTR_ARENA];
        let vlen = {
            let arena = &blkbuf[..xattr::F2FS_XATTR_ARENA];
            match xattr::find(arena, xattr::IDX_ACL_ACCESS, b"") {
                Some(v) => { vbuf[..v.len()].copy_from_slice(v); Some(v.len()) }
                None => None,
            }
        };
        if let Some(vlen) = vlen {
            xattr::acl_chmod_rewrite(&mut vbuf[..vlen], new_mode);
            let arena = &mut blkbuf[..xattr::F2FS_XATTR_ARENA];
            let _ = xattr::set(arena, xattr::IDX_ACL_ACCESS, b"", &vbuf[..vlen], 0);
            persist_xattr_block(ms, ino, xnid, &blkbuf);
        }
    }

    maybe_flush(ms);
    ok_reply()
}

/// chown(2) via path. `u32::MAX` for `uid`/`gid` means "leave unchanged" —
/// mirrors chown(2)'s `-1` and matches apply_chown's tmpfs handling in
/// servers/vfs/src/lib.rs.
///
/// See `handle_chmod` for `follow`; lchown(2) is the usual false case, and
/// arrives here as VFS_LCHOWN.
fn handle_chown(ms: &mut MountState, path_ptr: u64, uid: u32, gid: u32, follow: bool,
                euid: u32, egid: u32) -> Message {
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
    let ino = resolve_path_ex(ms, rel, follow);
    if ino == 0 { return err_reply(-2); }
    chown_inode(ms, ino, uid, gid, euid, egid)
}

/// fchown(2) — fd already resolved to an inode via the open-file table.
fn handle_fchown(ms: &mut MountState, file_id: u64, uid: u32, gid: u32,
                 euid: u32, egid: u32) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); }
    let ino = ms.open_files[slot].inode;
    chown_inode(ms, ino, uid, gid, euid, egid)
}

/// Mutate i_uid/i_gid in place and write the inode block back, same
/// nat_update + maybe_flush shape as chmod_inode/handle_link.
fn chown_inode(ms: &mut MountState, ino: u32, uid: u32, gid: u32,
               euid: u32, egid: u32) -> Message {
    let addr = nat_lookup(ms, ino);
    {
        let iblk = ms.cache.read(ms.dev, addr as u64);
        let owner = inode_uid(iblk);
        if euid != 0 {
            // Non-root: must own the file, may never hand it to someone else,
            // and may only set a group it belongs to. With no supplementary
            // groups, "belongs to" means egid.
            if euid != owner { return err_reply(-1); }               // EPERM
            if uid != u32::MAX && uid != owner { return err_reply(-1); }
            if gid != u32::MAX && gid != egid  { return err_reply(-1); }
        }
    }
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
    if !dir_remove_entry(ms, parent_ino, name) { return err_reply(-2); }
    // An empty directory has no other links (no child ".." points back), so
    // removing its name is always the last reference: reclaim its dentry
    // blocks and inode. A directory is never held open through this server's
    // fd table (opendir reads via getdents on a normal fd, closed promptly),
    // but guard anyway for symmetry with unlink.
    if !ino_is_open(ms, ino) {
        free_inode_data_and_nodes(ms, ino);
        free_block(ms, iblkaddr);
    }
    maybe_flush(ms);
    ok_reply()
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
    let old_size = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_size(iblk) };

    // Shrinking: release the blocks past the new end (and zero the sub-block
    // tail) so the space comes back. `truncate_to` handles the whole file at
    // length 0 and just the tail for a non-zero shrink; both walk the node tree
    // structurally and set the new i_size. Growing or same-size: nothing to
    // free — the tail beyond the old end reads as a hole until written — so
    // only i_size changes.
    if length < old_size {
        truncate_to(ms, ino, length);
    } else {
        let iblk = ms.cache.get_mut(ms.dev, iblkaddr as u64);
        w64(iblk, INO_SIZE, length);
        nat_update(ms, ino, iblkaddr);
    }
    maybe_flush(ms);
    ok_reply()
}

/// statfs — report this volume's real geometry from the superblock and the
/// active checkpoint.
///
/// The path argument is deliberately ignored: one F2FS server instance owns
/// exactly one volume, so the port the message arrived on already identifies
/// the filesystem being asked about.
///
/// Figures, and how exact each one is:
///
/// * `f_blocks` = `block_count - segment0_blkaddr`, byte-for-byte what Linux's
///   `f2fs_statfs` reports. Exact.
/// * `f_bfree`/`f_bavail` = `user_blocks - valid_blocks`, where `valid_blocks`
///   is a live sum of the per-segment SIT vblocks counts read through the block
///   cache (`sit_count_valid_blocks`). This is Linux's `valid_user_blocks`
///   model and is block-granular, so it moves the instant a block is allocated
///   or reclaimed — including for a free still sitting dirty in the SIT cache.
///   It replaced `free_segment_count * blocks_per_seg`, a mount-time-static
///   value that never reflected create/delete churn.
/// * `f_files` = total NAT entries, i.e. the inode capacity. Exact.
/// * `f_ffree` is capped at `f_bavail` because a new inode also costs a node
///   block; Linux applies the same cap on top of a live valid-node count we
///   don't have, so this over-estimates on a heavily populated volume.
fn handle_statfs(ms: &mut MountState, buf_ptr: u64) -> Message {
    if buf_ptr == 0 { return err_reply(-14); } // EFAULT

    let bps          = ms.sb.blocks_per_seg as u64;
    let user_blocks  = ms.sb.seg_cnt_main as u64 * bps;
    let total_blocks = ms.sb.block_count.saturating_sub(ms.sb.segment0_blkaddr as u64);
    // A superblock we failed to make sense of must still not report zero —
    // `df` silently drops any filesystem with f_blocks == 0.
    let total_blocks = if total_blocks == 0 { user_blocks.max(1) } else { total_blocks };

    let valid_blocks = sit_count_valid_blocks(ms);
    let free_blocks  = user_blocks.saturating_sub(valid_blocks);

    // Linux: total_node_count = (segment_count_nat / 2) * blocks_per_seg * NAT_ENTRY_PER_BLOCK.
    // Half the NAT area is the shadow copy and holds no live entries.
    let total_nodes = (ms.sb.seg_cnt_nat as u64 / 2) * bps * NAT_ENTRY_PER_BLK as u64;

    let vals = vfs_server::StatfsVals {
        f_type:  vfs_server::F2FS_MAGIC,
        bsize:   BLOCK_SIZE as u64,
        blocks:  total_blocks,
        bfree:   free_blocks,
        bavail:  free_blocks,
        files:   total_nodes,
        ffree:   total_nodes.min(free_blocks),
        // f_fsid only has to be stable and distinct per mount; `df` uses it to
        // recognise the same filesystem reached by two paths.
        fsid:    (ms.dev as u64 + 1) << 32 | ms.sb.root_ino as u64,
        namelen: 255, // F2FS_NAME_LEN
    };
    vfs_server::write_statfs(buf_ptr as usize, &vals);
    ok_reply()
}

// ── Extended attributes + POSIX ACLs ──────────────────────────────────────────
//
// Storage: one dedicated node block per inode, allocated lazily on the first
// setxattr that actually stores something. Bytes [0..F2FS_XATTR_ARENA=4076)
// hold the packed arena (all-zero == empty); [4076..4096) carry the standard
// node footer (nid, owner ino) exactly like every other node block. The inode
// records that node's nid at INO_XATTR (0 == none). The wire format, size caps,
// namespace gates and the ACL evaluator all live in the `xattr` crate — this
// module never reimplements them, it only does the read-modify-write against
// the node block, in the same nat_lookup → mutate → nat_update → maybe_flush
// idiom as chmod_inode.

/// The inode's owner/mode facts (for the `xattr` gates) plus its xattr nid.
fn load_meta_xnid(ms: &mut MountState, ino: u32) -> (xattr::FileMeta, u32) {
    let addr = nat_lookup(ms, ino);
    let iblk = ms.cache.read(ms.dev, addr as u64);
    let meta = xattr::FileMeta {
        mode: inode_mode(iblk), // on-disk i_mode carries the S_IFMT type bits
        uid:  inode_uid(iblk),
        gid:  inode_gid(iblk),
    };
    (meta, r32(iblk, INO_XATTR))
}

/// Copy the whole xattr node block (arena + footer) onto the stack. Copying
/// (rather than holding a `&` into the cache) is mandatory: the write-back path
/// takes `get_mut` on NAT/SIT blocks through the same 4-slot cache and can evict
/// the node block being edited.
fn read_xattr_arena(ms: &mut MountState, xnid: u32) -> [u8; BLOCK_SIZE] {
    let addr = nat_lookup(ms, xnid);
    read_block_copy(ms, addr)
}

/// Write a full xattr node-block image back, allocating the node lazily when the
/// inode has none yet. `blkbuf` is the 4096-byte block (arena in
/// [0..F2FS_XATTR_ARENA)); its footer is (re)written here so a freshly
/// allocated, zero-filled buffer becomes a self-identifying node block. Returns
/// the effective nid, or 0 if a lazy allocation failed (ENOSPC) — callers treat
/// 0 as "nothing was stored".
fn persist_xattr_block(ms: &mut MountState, ino: u32, xnid: u32, blkbuf: &[u8; BLOCK_SIZE]) -> u32 {
    let xnid = if xnid != 0 {
        xnid
    } else {
        // Allocate the node first and hold no cache borrow across it — the
        // allocation itself dirties SIT/NAT blocks through the shared cache.
        let (new_nid, _phys) = match create_node_block(ms, ino) {
            Some(v) => v,
            None => return 0, // ENOSPC
        };
        // Record the new nid in the inode as a separate, non-straddling step.
        let iaddr = nat_lookup(ms, ino);
        {
            let iblk = ms.cache.get_mut(ms.dev, iaddr as u64);
            w32(iblk, INO_XATTR, new_nid);
        }
        nat_update(ms, ino, iaddr);
        new_nid
    };
    let xaddr = nat_lookup(ms, xnid);
    let mut out = *blkbuf;
    w32(&mut out, NODE_FOOTER_OFF,     xnid);
    w32(&mut out, NODE_FOOTER_OFF + 4, ino);
    ms.cache.write(ms.dev, xaddr as u64, &out);
    nat_update(ms, xnid, xaddr);
    xnid
}

/// Replace only i_mode's 9 permission bits, keeping type/setuid/setgid/sticky —
/// the mode<->ACL invariant (`acl_mode_bits`) that setxattr and chmod maintain.
fn set_inode_perm_bits(ms: &mut MountState, ino: u32, perm: u16) {
    let addr = nat_lookup(ms, ino);
    {
        let iblk = ms.cache.get_mut(ms.dev, addr as u64);
        let cur = inode_mode(iblk);
        w16(iblk, INO_MODE, (cur & !0o777) | (perm & 0o777));
    }
    nat_update(ms, ino, addr);
}

/// Copy a NUL-terminated attribute name out of caller space into `buf`.
/// Returns the length read, capped at `buf.len()`; a name that fills `buf`
/// without a terminator is over-length and rejected by the caller.
unsafe fn read_user_name(name_ptr: u64, buf: &mut [u8]) -> usize {
    let ptr = name_ptr as *const u8;
    let mut len = 0;
    while len < buf.len() {
        let c = *ptr.add(len);
        if c == 0 { break; }
        buf[len] = c;
        len += 1;
    }
    len
}

/// Fill `buf` (must be ≥ XATTR_NAME_MAX+1 bytes) with the caller's attribute
/// name and validate its length. ERANGE for empty or over-long names.
unsafe fn load_xattr_name(name_ptr: u64, buf: &mut [u8]) -> Result<usize, i32> {
    let n = read_user_name(name_ptr, buf);
    if n == 0 || n > xattr::XATTR_NAME_MAX { return Err(xattr::ERANGE); }
    Ok(n)
}

/// Resolve a path-form xattr op to an inode. `follow` distinguishes the
/// bare form (follow the final symlink) from the l-form (do not).
fn xattr_path_ino(ms: &mut MountState, path_ptr: u64, follow: bool) -> Result<u32, Message> {
    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0;
        while *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };
    let rel = match get_relative_path(ms, path_bytes) {
        Some(r) => r,
        None => return Err(err_reply(-2)), // ENOENT
    };
    let ino = resolve_path_ex(ms, rel, follow);
    if ino == 0 { return Err(err_reply(-2)); }
    Ok(ino)
}

/// Resolve an f-form xattr op to an inode via the open-file table (arg0 is the
/// mount-local file_id the VFS forwarded), exactly like handle_fchmod.
fn xattr_fd_ino(ms: &MountState, file_id: u64) -> Result<u32, Message> {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use {
        return Err(err_reply(-9)); // EBADF
    }
    Ok(ms.open_files[slot].inode)
}

fn xattr_get(ms: &mut MountState, ino: u32, name_ptr: u64, val_ptr: u64, size: u64,
             euid: u32, egid: u32) -> Message {
    let mut namebuf = [0u8; xattr::XATTR_NAME_MAX + 1];
    let nlen = match unsafe { load_xattr_name(name_ptr, &mut namebuf) } {
        Ok(n) => n,
        Err(e) => return err_reply(-e),
    };
    let (idx, suf) = match xattr::split_name(&namebuf[..nlen]) {
        Some(v) => v,
        None => return err_reply(-95), // EOPNOTSUPP
    };
    let (meta, xnid) = load_meta_xnid(ms, ino);
    if xnid == 0 {
        // Gate still runs (it can return EACCES/EPERM/EOPNOTSUPP), then ENODATA.
        if let Err(e) = xattr::may_read_xattr(idx, &meta, euid, egid, None) { return err_reply(-e); }
        return err_reply(-61); // ENODATA
    }
    let blkbuf = read_xattr_arena(ms, xnid);
    let arena = &blkbuf[..xattr::F2FS_XATTR_ARENA];
    let acl = xattr::find(arena, xattr::IDX_ACL_ACCESS, b"");
    if let Err(e) = xattr::may_read_xattr(idx, &meta, euid, egid, acl) { return err_reply(-e); }
    let val = match xattr::find(arena, idx, suf) {
        Some(v) => v,
        None => return err_reply(-61), // ENODATA
    };
    if size == 0 { return val_reply(val.len() as u64); }
    if (val.len() as u64) > size { return err_reply(-34); } // ERANGE
    unsafe { core::ptr::copy_nonoverlapping(val.as_ptr(), val_ptr as *mut u8, val.len()); }
    val_reply(val.len() as u64)
}

fn xattr_list(ms: &mut MountState, ino: u32, list_ptr: u64, size: u64, euid: u32) -> Message {
    let (_meta, xnid) = load_meta_xnid(ms, ino);
    // ls -l fast path: no xattr node means an empty list, answered without a
    // block read.
    if xnid == 0 { return val_reply(0); }
    let blkbuf = read_xattr_arena(ms, xnid);
    let arena = &blkbuf[..xattr::F2FS_XATTR_ARENA];
    let empty: &mut [u8] = &mut [];
    let out = if size == 0 {
        empty
    } else {
        unsafe { core::slice::from_raw_parts_mut(list_ptr as *mut u8, size as usize) }
    };
    match xattr::list(arena, out, euid == 0) {
        Ok(n) => val_reply(n as u64),
        Err(e) => err_reply(-e),
    }
}

fn xattr_set(ms: &mut MountState, ino: u32, name_ptr: u64, val_ptr: u64, size: u64,
             flags: u64, euid: u32, egid: u32) -> Message {
    let mut namebuf = [0u8; xattr::XATTR_NAME_MAX + 1];
    let nlen = match unsafe { load_xattr_name(name_ptr, &mut namebuf) } {
        Ok(n) => n,
        Err(e) => return err_reply(-e),
    };
    let (idx, suf) = match xattr::split_name(&namebuf[..nlen]) {
        Some(v) => v,
        None => return err_reply(-95), // EOPNOTSUPP
    };
    let (meta, xnid) = load_meta_xnid(ms, ino);

    // The current arena (all-zero when no node exists yet) — both the gate and
    // the read-modify-write operate on it.
    let mut blkbuf = if xnid != 0 { read_xattr_arena(ms, xnid) } else { [0u8; BLOCK_SIZE] };
    {
        let arena = &blkbuf[..xattr::F2FS_XATTR_ARENA];
        let acl = if xnid != 0 { xattr::find(arena, xattr::IDX_ACL_ACCESS, b"") } else { None };
        if let Err(e) = xattr::may_write_xattr(idx, &meta, euid, egid, acl) { return err_reply(-e); }
    }

    // The kernel prefaulted the value; forward the raw span verbatim.
    let val = unsafe { core::slice::from_raw_parts(val_ptr as *const u8, size as usize) };

    if idx == xattr::IDX_ACL_ACCESS || idx == xattr::IDX_ACL_DEFAULT {
        // A zero-length value removes the stored attribute; absent is still OK.
        if size == 0 {
            if xnid != 0 {
                {
                    let arena = &mut blkbuf[..xattr::F2FS_XATTR_ARENA];
                    let _ = xattr::remove(arena, idx, suf);
                }
                persist_xattr_block(ms, ino, xnid, &blkbuf);
                maybe_flush(ms);
            }
            return ok_reply();
        }
        let summary = match xattr::acl_validate(val) {
            Ok(s) => s,
            Err(_) => return err_reply(-22), // EINVAL
        };
        if idx == xattr::IDX_ACL_ACCESS {
            let perm = xattr::acl_mode_bits(&summary);
            if xattr::acl_is_trivial(&summary) {
                // Representable as mode bits alone: fold into i_mode, store
                // nothing, and drop any previously stored access ACL.
                set_inode_perm_bits(ms, ino, perm);
                if xnid != 0 {
                    {
                        let arena = &mut blkbuf[..xattr::F2FS_XATTR_ARENA];
                        let _ = xattr::remove(arena, xattr::IDX_ACL_ACCESS, b"");
                    }
                    persist_xattr_block(ms, ino, xnid, &blkbuf);
                }
                maybe_flush(ms);
                return ok_reply();
            }
            // Non-trivial: store the ACL, then sync i_mode's perm bits to it.
            {
                let arena = &mut blkbuf[..xattr::F2FS_XATTR_ARENA];
                if let Err(e) = xattr::set(arena, idx, suf, val, flags as u32) { return err_reply(-e); }
            }
            if persist_xattr_block(ms, ino, xnid, &blkbuf) == 0 { return err_reply(-28); } // ENOSPC
            set_inode_perm_bits(ms, ino, perm);
            maybe_flush(ms);
            return ok_reply();
        }
        // Default ACL: directories only (enforced by the gate), stored verbatim.
        {
            let arena = &mut blkbuf[..xattr::F2FS_XATTR_ARENA];
            if let Err(e) = xattr::set(arena, idx, suf, val, flags as u32) { return err_reply(-e); }
        }
        if persist_xattr_block(ms, ino, xnid, &blkbuf) == 0 { return err_reply(-28); }
        maybe_flush(ms);
        return ok_reply();
    }

    // Non-ACL namespace: a plain arena insert/replace.
    {
        let arena = &mut blkbuf[..xattr::F2FS_XATTR_ARENA];
        if let Err(e) = xattr::set(arena, idx, suf, val, flags as u32) { return err_reply(-e); }
    }
    if persist_xattr_block(ms, ino, xnid, &blkbuf) == 0 { return err_reply(-28); }
    maybe_flush(ms);
    ok_reply()
}

fn xattr_remove(ms: &mut MountState, ino: u32, name_ptr: u64, euid: u32, egid: u32) -> Message {
    let mut namebuf = [0u8; xattr::XATTR_NAME_MAX + 1];
    let nlen = match unsafe { load_xattr_name(name_ptr, &mut namebuf) } {
        Ok(n) => n,
        Err(e) => return err_reply(-e),
    };
    let (idx, suf) = match xattr::split_name(&namebuf[..nlen]) {
        Some(v) => v,
        None => return err_reply(-95), // EOPNOTSUPP
    };
    let (meta, xnid) = load_meta_xnid(ms, ino);
    let mut blkbuf = if xnid != 0 { read_xattr_arena(ms, xnid) } else { [0u8; BLOCK_SIZE] };
    {
        let arena = &blkbuf[..xattr::F2FS_XATTR_ARENA];
        let acl = if xnid != 0 { xattr::find(arena, xattr::IDX_ACL_ACCESS, b"") } else { None };
        if let Err(e) = xattr::may_write_xattr(idx, &meta, euid, egid, acl) { return err_reply(-e); }
    }
    if xnid == 0 { return err_reply(-61); } // ENODATA
    let removed = {
        let arena = &mut blkbuf[..xattr::F2FS_XATTR_ARENA];
        xattr::remove(arena, idx, suf)
    };
    match removed {
        Ok(_) => {
            // The block stays allocated even when it empties out — nids are
            // never recycled on this volume, consistent with unlink/truncate.
            persist_xattr_block(ms, ino, xnid, &blkbuf);
            maybe_flush(ms);
            ok_reply()
        }
        Err(e) => err_reply(-e), // ENODATA
    }
}

/// VFS_ACCESS(path_ptr, amode) — faccessat routed to the owning filesystem so a
/// stored POSIX ACL is honoured. amode == 0 (F_OK) is pure existence.
fn xattr_access(ms: &mut MountState, path_ptr: u64, amode: u64, euid: u32, egid: u32) -> Message {
    let ino = match xattr_path_ino(ms, path_ptr, true) {
        Ok(i) => i,
        Err(m) => return m,
    };
    if amode == 0 { return ok_reply(); }
    let (meta, xnid) = load_meta_xnid(ms, ino);
    let xbuf = if xnid != 0 { Some(read_xattr_arena(ms, xnid)) } else { None };
    let acl = match &xbuf {
        Some(b) => xattr::find(&b[..xattr::F2FS_XATTR_ARENA], xattr::IDX_ACL_ACCESS, b""),
        None => None,
    };
    if xattr::access_check(&meta, euid, egid, acl, amode & 4 != 0, amode & 2 != 0, amode & 1 != 0) {
        ok_reply()
    } else {
        err_reply(-13) // EACCES
    }
}

// ── IPC dispatch ──────────────────────────────────────────────────────────────

fn f2fs_dispatch(msg: &Message, caller_pid: u32, target_port: u32) -> Message {
    let mut mounts = F2FS_MOUNTS.lock();
    for slot in mounts.iter_mut() {
        if let Some(ref mut ms) = slot {
            if ms.port == target_port {
                return dispatch_msg(ms, msg, caller_pid);
            }
        }
    }
    err_reply(-5) // EIO — no mount found for this port
}

/// Effective uid/gid of the process that made the call.
///
/// `port::send` invokes handlers synchronously in the caller's own task
/// context and passes its pid, so this needs no protocol change — the value
/// was already on the wire, it was simply discarded.
///
/// Note `sched::euid_of` answers 0 for a pid it cannot find, i.e. it fails
/// *open* to root. That is the right answer for the boot-time mount path,
/// which runs before there is a user process to attribute, but it is a
/// deliberate choice rather than an accident.
fn caller_creds(pid: u32) -> (u32, u32) {
    (sched::euid_of(pid), sched::egid_of(pid))
}

fn dispatch_msg(ms: &mut MountState, msg: &Message, caller_pid: u32) -> Message {
    let (euid, egid) = caller_creds(caller_pid);
    match msg.tag {
        VFS_OPEN       => handle_open(ms, arg(msg,0), arg(msg,1), arg(msg,2), euid, egid),
        VFS_READ       => handle_read(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_WRITE      => handle_write(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_CLOSE      => handle_close(ms, arg(msg,0)),
        VFS_LSEEK      => handle_lseek(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_STAT       => handle_stat(ms, arg(msg,0), arg(msg,1)),
        VFS_FSTAT      => handle_fstat(ms, arg(msg,0), arg(msg,1)),
        VFS_GETDENTS64 => handle_getdents(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_MKDIR      => handle_mkdir(ms, arg(msg,0), arg(msg,1), euid, egid),
        VFS_UNLINK     => handle_unlink(ms, arg(msg,0)),
        VFS_RMDIR      => handle_rmdir(ms, arg(msg,0)),
        VFS_RENAME     => handle_rename(ms, arg(msg,0), arg(msg,1)),
        VFS_FTRUNCATE  => handle_ftruncate(ms, arg(msg,0), arg(msg,1)),
        VFS_STATFS     => handle_statfs(ms, arg(msg,1)),
        VFS_LSTAT      => handle_lstat(ms, arg(msg,0), arg(msg,1)),
        VFS_SYMLINK    => handle_symlink(ms, arg(msg,0), arg(msg,1), euid, egid),
        VFS_FD_PATH    => handle_fd_path(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_READLINK   => handle_readlink(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_LINK       => handle_link(ms, arg(msg,0), arg(msg,1)),
        VFS_CHMOD      => handle_chmod(ms, arg(msg,0), arg(msg,1) as u32, true, euid),
        VFS_LCHMOD     => handle_chmod(ms, arg(msg,0), arg(msg,1) as u32, false, euid),
        VFS_FCHMOD     => handle_fchmod(ms, arg(msg,0), arg(msg,1) as u32, euid),
        VFS_FSYNC      => handle_fsync(ms),
        VFS_CHOWN      => handle_chown(ms, arg(msg,0), arg(msg,1) as u32, arg(msg,2) as u32, true, euid, egid),
        VFS_LCHOWN     => handle_chown(ms, arg(msg,0), arg(msg,1) as u32, arg(msg,2) as u32, false, euid, egid),
        VFS_FCHOWN     => handle_fchown(ms, arg(msg,0), arg(msg,1) as u32, arg(msg,2) as u32, euid, egid),

        // Extended attributes. Path forms carry (path, name, value, size, flags);
        // f-forms replace path with a mount-local file_id. The l-forms differ
        // only in not following a final symlink. Args past the third are read
        // from the same inline payload (Message.data holds 55 u64 words).
        VFS_SETXATTR | VFS_LSETXATTR =>
            match xattr_path_ino(ms, arg(msg,0), msg.tag == VFS_SETXATTR) {
                Ok(ino) => xattr_set(ms, ino, arg(msg,1), arg(msg,2), arg(msg,3), arg(msg,4), euid, egid),
                Err(m) => m,
            },
        VFS_FSETXATTR =>
            match xattr_fd_ino(ms, arg(msg,0)) {
                Ok(ino) => xattr_set(ms, ino, arg(msg,1), arg(msg,2), arg(msg,3), arg(msg,4), euid, egid),
                Err(m) => m,
            },
        VFS_GETXATTR | VFS_LGETXATTR =>
            match xattr_path_ino(ms, arg(msg,0), msg.tag == VFS_GETXATTR) {
                Ok(ino) => xattr_get(ms, ino, arg(msg,1), arg(msg,2), arg(msg,3), euid, egid),
                Err(m) => m,
            },
        VFS_FGETXATTR =>
            match xattr_fd_ino(ms, arg(msg,0)) {
                Ok(ino) => xattr_get(ms, ino, arg(msg,1), arg(msg,2), arg(msg,3), euid, egid),
                Err(m) => m,
            },
        VFS_LISTXATTR | VFS_LLISTXATTR =>
            match xattr_path_ino(ms, arg(msg,0), msg.tag == VFS_LISTXATTR) {
                Ok(ino) => xattr_list(ms, ino, arg(msg,1), arg(msg,2), euid),
                Err(m) => m,
            },
        VFS_FLISTXATTR =>
            match xattr_fd_ino(ms, arg(msg,0)) {
                Ok(ino) => xattr_list(ms, ino, arg(msg,1), arg(msg,2), euid),
                Err(m) => m,
            },
        VFS_REMOVEXATTR | VFS_LREMOVEXATTR =>
            match xattr_path_ino(ms, arg(msg,0), msg.tag == VFS_REMOVEXATTR) {
                Ok(ino) => xattr_remove(ms, ino, arg(msg,1), euid, egid),
                Err(m) => m,
            },
        VFS_FREMOVEXATTR =>
            match xattr_fd_ino(ms, arg(msg,0)) {
                Ok(ino) => xattr_remove(ms, ino, arg(msg,1), euid, egid),
                Err(m) => m,
            },
        VFS_ACCESS     => xattr_access(ms, arg(msg,0), arg(msg,1), euid, egid),

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

