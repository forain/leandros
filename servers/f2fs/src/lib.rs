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

use core::sync::atomic;
use alloc::boxed::Box;
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
const VFS_FALLOCATE:  u64 = vfs_server::VFS_FALLOCATE;
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
const VFS_UTIMENS:      u64 = vfs_server::VFS_UTIMENS;
const VFS_LUTIMENS:     u64 = vfs_server::VFS_LUTIMENS;
const VFS_FUTIMENS:     u64 = vfs_server::VFS_FUTIMENS;

const O_WRONLY:  u64 = 1;
const O_RDWR:    u64 = 2;
const O_CREAT:   u64 = 0o100;
const O_EXCL:    u64 = 0o200;
const O_TRUNC:   u64 = 0o1000;
/// Present here only for documentation: the VFS does **not** latch O_APPEND
/// into the open-file slot, it sets arg3 of every VFS_WRITE for an appending
/// descriptor (see `handle_write`). Latching it at open would be wrong twice
/// over — `fcntl(F_SETFL)` can add the bit afterwards, and a latched flag
/// invites "seek to EOF once at open", which is not what O_APPEND means.
#[allow(dead_code)]
const O_APPEND:  u64 = 0o2000;
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
/// `__le32 ckpt_flags`. Only `CP_UMOUNT_FLAG` is used here: Linux sets it on
/// the checkpoint written by a clean unmount (`f2fs_put_super` ->
/// `CP_UMOUNT`) and clears it on every other one, so its absence on the
/// newest pack at mount time is exactly "the volume was not shut down
/// cleanly" (Linux then runs roll-forward recovery).
const CP_CKPT_FLAGS:     usize = 132;
const CP_UMOUNT_FLAG:    u32   = 0x0000_0001;

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
/// `i_advise` — one byte of per-inode flags, zero on every inode mkfs writes.
const INO_ADVISE:    usize = 2;
/// `i_blocks` (u64): 4 KiB blocks charged to this inode, data *and* node. Only
/// meaningful when `INO_ADVISE` carries `F2FS_ADVISE_IBLOCKS`. Offsets 28..84
/// are unused by both mkfs scripts and by `create_inode`.
const INO_BLOCKS:    usize = 28;
/// Timestamps: `{ u64 sec, u32 nsec }` × atime/mtime/ctime in the otherwise
/// unused 36..72 span (this volume format is not Linux's, whose i_atime sits
/// at 32). Every inode mkfs writes reads back 0 here — the epoch, which is
/// exactly what stat reported for everything before timestamps existed.
const INO_ATIME:     usize = 36;
const INO_ATIME_NS:  usize = 44;
const INO_MTIME:     usize = 48;
const INO_MTIME_NS:  usize = 56;
const INO_CTIME:     usize = 60;
const INO_CTIME_NS:  usize = 68;
const INO_NAMELEN:   usize = 88;
const INO_NAME:      usize = 92;   // [u8; 255]
// The union (i_addr / extra-attrs) starts here:
const INODE_UNION:   usize = 364;
// i_nid[5] start, footer at NODE_FOOTER_OFF
const INODE_NIDS_OFF:usize = 4056;
const NODE_FOOTER_OFF:usize = 4076; // footer = 5×u32 = 20 bytes

// F2FS_INLINE flags
const F2FS_EXTRA_ATTR:    u8 = 0x20;

/// `i_advise` bit meaning "`i_blocks` at `INO_BLOCKS` is maintained for this
/// inode". Inodes this server creates set it; the ones the two mkfs scripts
/// bake into the image do not, and those are all fully allocated, so `stat`
/// falls back to deriving the count from `i_size` for them — which for a dense
/// file is the right answer anyway. Without the bit there is no way to tell
/// "sparse file, 3 blocks" from "pre-existing inode, count never kept".
const F2FS_ADVISE_IBLOCKS: u8 = 0x80;

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
//
// History: 4 slots could not hold one path component's working set (every
// stat of a missing icon path cost ~2.8 ms of reads); 192 slots (768 KiB)
// fixed lookups but not write-back. On the PC's USB stick (nvrm-boot12.log)
// the 192-slot cache was the bottleneck of the whole system: a 3/4-dirty
// cache forced a checkpoint with the filesystem lock held (11 of 14 `[HUNG]`
// reports, 5-6 s each), an eviction of a dirty slot wrote it synchronously
// under the lock, and the linear `find` cost 192 compares per lookup.
//
// Now (lane f2fsperf):
// - up to `cache_cap()` slots (64 MiB on a machine with >= 2 GiB, RAM/32
//   below that, never under 4 MiB), allocated lazily in 1 MiB chunks, so a
//   mount only pays for its working set;
// - a hash index (`find` is O(1)) and an intrusive LRU list holding exactly
//   the slots that may be evicted: clean and not pinned. Dirty slots and
//   slots an unlocked checkpoint is writing (`wb`) are not on it, so
//   eviction is O(1) and never writes anything;
// - dirty data never reaches the medium outside a checkpoint: dirty
//   pressure only REQUESTS an unlocked checkpoint (`soft_dirty`), a writer
//   that pushes the pinned count past `hard_pinned` runs one itself after
//   releasing the lock (throttle), and only if a single request finds every
//   slot pinned does the cache grow past its cap (up to 2x) and, past that,
//   write one block back in place under the lock (`[F2FS] block cache
//   exhausted`, logged: it should never happen).
const CACHE_CHUNK: usize = 256;
const CACHE_MAX_SLOTS: usize = 16384;
const CACHE_MIN_SLOTS: usize = 1024;
const NULL_BLK: u64 = u64::MAX;
const NIL: u32 = u32::MAX;
/// Dirty data that makes the cache ask for a checkpoint at once (16 MiB).
const DIRTY_SOFT_BYTES: usize = 16 << 20;
/// Longest run of adjacent blocks a write-back coalesces: 960 KiB, a multiple
/// of the USB MSC 120 KiB command (`MAX_CHUNK_BYTES`), so a long run becomes
/// whole commands (the old 32 blocks = 128 KiB went out as 120 + 8 KiB, an
/// extra small write per run). `flush_all` issues a run as one request (the
/// MSC and virtio-blk drivers split it); `checkpoint_unlocked` issues it in
/// `WB_IO_BLOCKS` pieces.
const MAX_RUN: usize = 240;
/// Blocks per device request in an unlocked checkpoint's write phase: 120 KiB,
/// exactly one USB MSC command (see `checkpoint_unlocked`).
const WB_IO_BLOCKS: usize = 30;

/// Slots for a mount's block cache: 1/32 of RAM, within [4, 64] MiB.
/// `f2fs.cache_kib=N` on the kernel command line overrides the size (same
/// bounds): a small cache makes metadata misses common, which is how the
/// regression probes exercise the unlocked-miss path (lane nostall).
fn cache_cap() -> usize {
    let ram = mm::buddy::total_pages().saturating_mul(4096);
    let slots = match drivers::nvidia_probe::cmdline_opt("f2fs.cache_kib") {
        Some(kib) if kib > 0 => (kib as usize) * 1024 / BLOCK_SIZE,
        _ => ram / 32 / BLOCK_SIZE,
    };
    let slots = slots.clamp(CACHE_MIN_SLOTS, CACHE_MAX_SLOTS);
    slots / CACHE_CHUNK * CACHE_CHUNK
}

#[derive(Clone, Copy)]
struct SlotMeta {
    blk_no: u64,
    dirty: bool,
    /// Clean in the cache but its contents are being written by an unlocked
    /// checkpoint (`checkpoint_unlocked`, phase 2): pinned like a dirty slot
    /// (the device may still hold an older copy) until phase 3 unpins it.
    wb: bool,
    /// Hash chain.
    hnext: u32,
    /// LRU list (only while clean and not pinned).
    prev: u32,
    next: u32,
    /// `BlockCache::ep` of the request that last logged this slot for undo
    /// (0 = never; reset when the slot changes blocks).
    uep: u64,
}

struct BlockCache {
    meta: alloc::vec::Vec<SlotMeta>,
    chunks: alloc::vec::Vec<Box<[u8]>>,
    buckets: alloc::vec::Vec<u32>,
    hash_shift: u32,
    /// Soft cap (slots); the cache grows past it only when every slot is pinned.
    cap: usize,
    /// Evictable list: most recently used at `head`, never-used slots at the
    /// tail end.
    head: u32,
    tail: u32,
    /// Slots dirty.
    ndirty: usize,
    /// Slots dirty or `wb` (not evictable).
    npinned: usize,
    exhausted: u32,
    /// No-I/O request mode (lane nostall, see `with_mount`): `PROBE_OFF`,
    /// `PROBE_ON` (a miss is recorded instead of read) or `PROBE_MISSED`
    /// (a miss happened: the request's result will be discarded and its
    /// changes undone, so from here on nothing real is changed).
    probe: u8,
    /// The misses of the current request (lane fsyncbatch): the first one,
    /// then every further distinct block the doomed run asked for, up to
    /// `MISS_BATCH`, as `(first block, block count)`. `with_mount` fetches
    /// them all in one unlocked pass, so a cold path waits for the device
    /// once instead of once per block.
    misses: [(u64, u32); MISS_BATCH],
    nmiss: usize,
    /// Blocks on the device (misses at or past it are dropped: a doomed run
    /// walks zero-filled blocks and can compute any address).
    dev_blocks: u64,
    /// Changes made to cached blocks (`write`, `get_mut`), ever: lets
    /// `tracked` notice a request that changed the volume without saying
    /// which file it changed (lane fsyncbatch).
    dirtied: u64,
    /// What a missed block reads as: always zeros, never handed out mutably.
    scratch_r: Box<[u8; BLOCK_SIZE]>,
    /// What a change to a block reads/writes once the request is doomed:
    /// zeroed at every hand-out, never read back as a real block.
    scratch_w: Box<[u8; BLOCK_SIZE]>,
    /// Undo log of the current request: every slot it changed, with the
    /// slot's previous flags and (`data_off`) contents in `undo_data`, or
    /// `UNDO_NEW` for a slot it installed.
    undo: alloc::vec::Vec<Undo>,
    undo_data: alloc::vec::Vec<u8>,
    /// Current request's undo epoch (never 0; 64 bits so it never wraps
    /// back onto a slot's stale `uep`).
    ep: u64,
    /// Install generation per block (hashed): bumped whenever a block enters
    /// the cache, so an unlocked read can tell that the block was cached (and
    /// possibly written back and dropped) while it was reading the device.
    gens: alloc::vec::Vec<u32>,
}

const PROBE_OFF: u8 = 0;
const PROBE_ON: u8 = 1;
const PROBE_MISSED: u8 = 2;
const UNDO_NEW: usize = usize::MAX;
const GEN_BLK_SLOTS: usize = 4096;
/// Largest run of blocks one unlocked miss fetch reads (data-run misses).
const FETCH_MAX: usize = 32;
/// Distinct misses one doomed run records for a single unlocked fetch pass.
const MISS_BATCH: usize = 16;
/// Blocks of gap a fetch pass reads through to join two misses into one
/// device command (a few KiB more transfer instead of one more command,
/// which on the USB stick can queue behind a 0.25-1 s write).
const FETCH_GAP: u64 = 4;

/// The misses of one doomed run (`BlockCache::misses`).
type MissList = ([(u64, u32); MISS_BATCH], usize);

#[derive(Clone, Copy)]
struct Undo { i: u32, blk: u64, dirty: bool, wb: bool, data_off: usize }

impl BlockCache {
    fn new(cap: usize) -> Self {
        let nb = (cap * 2).next_power_of_two().max(1024);
        Self {
            meta: alloc::vec::Vec::new(),
            chunks: alloc::vec::Vec::new(),
            buckets: alloc::vec![NIL; nb],
            hash_shift: 64 - nb.trailing_zeros(),
            cap,
            head: NIL,
            tail: NIL,
            ndirty: 0,
            npinned: 0,
            exhausted: 0,
            probe: PROBE_OFF,
            misses: [(0, 0); MISS_BATCH],
            nmiss: 0,
            dev_blocks: u64::MAX,
            dirtied: 0,
            scratch_r: Box::new([0u8; BLOCK_SIZE]),
            scratch_w: Box::new([0u8; BLOCK_SIZE]),
            undo: alloc::vec::Vec::new(),
            undo_data: alloc::vec::Vec::new(),
            ep: 1,
            gens: alloc::vec![0u32; GEN_BLK_SLOTS],
        }
    }

    // ── No-I/O requests (lane nostall) ──────────────────────────────────────

    #[inline]
    fn poisoned(&self) -> bool { self.probe == PROBE_MISSED }

    #[inline]
    fn gen_idx(blk: u64) -> usize {
        (blk.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 52) as usize % GEN_BLK_SLOTS
    }

    /// Install generation of `blk` (see `gens`).
    fn blk_gen(&self, blk: u64) -> u32 { self.gens[Self::gen_idx(blk)] }

    /// A request starts in no-I/O mode.
    fn probe_start(&mut self) {
        self.probe = PROBE_ON;
        self.ep += 1;
        self.undo.clear();
        self.undo_data.clear();
    }

    /// The request needs `n` blocks at `blk` that are not cached: remember
    /// the miss and doom the request. A doomed request keeps running on
    /// zero-filled blocks (lane nostall); every further distinct miss it
    /// makes is recorded too (lane fsyncbatch), up to `MISS_BATCH`, so the
    /// fetch pass reads them together. Later misses may be addresses
    /// computed from zeros rather than blocks the rerun will need: they cost
    /// a read, never correctness (each is installed clean, under the same
    /// generation rule as the first). Block 0 (the superblock, what a
    /// zeroed pointer reads) and addresses past the device are skipped.
    fn note_miss(&mut self, blk: u64, n: usize) {
        let n = n.clamp(1, FETCH_MAX) as u32;
        match self.probe {
            PROBE_ON => {
                self.misses[0] = (blk, n);
                self.nmiss = 1;
                self.probe = PROBE_MISSED;
            }
            PROBE_MISSED => {
                if !FSYNCBATCH_ON.load(atomic::Ordering::Relaxed) { return; }
                if blk == 0 || blk >= self.dev_blocks || self.nmiss >= MISS_BATCH { return; }
                let end = blk + n as u64;
                if self.misses[..self.nmiss].iter().any(|&(b, m)| blk >= b && end <= b + m as u64) {
                    return;
                }
                self.misses[self.nmiss] = (blk, n);
                self.nmiss += 1;
                NOIO_MISS_EXTRA.fetch_add(1, atomic::Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// Give up no-I/O mode for the rest of this request (out of memory for
    /// the undo log, or an undo the log cannot express). Only possible before
    /// a miss, so nothing ever has to be undone; misses from here on are read
    /// with the lock held, as before lane nostall (counted).
    fn probe_abandon(&mut self) {
        if self.probe == PROBE_ON {
            self.probe = PROBE_OFF;
            NOIO_ABANDONED.fetch_add(1, atomic::Ordering::Relaxed);
        }
    }

    /// Record slot `i`'s state before the current request changes it (once
    /// per request; `new` = the request installed it).
    fn log_slot(&mut self, i: usize, new: bool) {
        if self.probe != PROBE_ON || self.meta[i].uep == self.ep { return; }
        if self.undo.try_reserve(1).is_err() { self.probe_abandon(); return; }
        let data_off = if new { UNDO_NEW } else {
            if self.undo_data.try_reserve(BLOCK_SIZE).is_err() { self.probe_abandon(); return; }
            let o = self.undo_data.len();
            let c = (i % CACHE_CHUNK) * BLOCK_SIZE;
            self.undo_data.extend_from_slice(&self.chunks[i / CACHE_CHUNK][c..c + BLOCK_SIZE]);
            o
        };
        let m = self.meta[i];
        self.undo.push(Undo { i: i as u32, blk: m.blk_no, dirty: m.dirty, wb: m.wb, data_off });
        self.meta[i].uep = self.ep;
    }

    /// The request ended. If it missed: undo every change it made to the
    /// cache (newest first) and return the misses to fetch.
    fn probe_stop(&mut self) -> Option<MissList> {
        let missed = self.probe == PROBE_MISSED;
        self.probe = PROBE_OFF;
        if missed {
            for k in (0..self.undo.len()).rev() {
                let u = self.undo[k];
                let i = u.i as usize;
                // Evicted (a clean slot changed in place) and reused since:
                // its block was clean, so the device copy stands.
                if self.meta[i].blk_no != u.blk { continue; }
                if u.data_off == UNDO_NEW {
                    self.set_flags(i, false, false);
                    self.hash_remove(i);
                    self.meta[i].blk_no = NULL_BLK;
                    self.lru_unlink(i);
                    self.lru_push_back(i);
                } else {
                    let c = (i % CACHE_CHUNK) * BLOCK_SIZE;
                    self.chunks[i / CACHE_CHUNK][c..c + BLOCK_SIZE]
                        .copy_from_slice(&self.undo_data[u.data_off..u.data_off + BLOCK_SIZE]);
                    self.set_flags(i, u.dirty, u.wb);
                }
                self.meta[i].uep = 0;
            }
        }
        self.undo.clear();
        if self.undo_data.capacity() > (4 << 20) { self.undo_data = alloc::vec::Vec::new(); }
        self.undo_data.clear();
        if missed { Some((self.misses, self.nmiss)) } else { None }
    }

    /// A doomed request's view of a block it changes.
    fn scratch_mut(&mut self) -> &mut [u8; BLOCK_SIZE] {
        self.scratch_w.fill(0);
        &mut *self.scratch_w
    }

    /// Install `data` (just read from the device, unlocked) as the clean
    /// copy of `blk`, unless the cache has the block already or it entered
    /// the cache since `gen` was taken (it may have been written back and
    /// dropped meanwhile, so what was read could predate that write).
    fn install_fetched(&mut self, dev: usize, blk: u64, data: &[u8], gen: u32) -> bool {
        if self.find(blk).is_some() { return true; }
        if self.blk_gen(blk) != gen { return false; }
        let i = self.take_slot(dev);
        self.data_mut(i).copy_from_slice(&data[..BLOCK_SIZE]);
        self.install(i, blk);
        true
    }

    /// Dirty slots at which a checkpoint is requested (16 MiB or cap/4).
    fn soft_dirty(&self) -> usize { (DIRTY_SOFT_BYTES / BLOCK_SIZE).min(self.cap / 4) }
    /// Pinned slots at which a writer must run a checkpoint itself.
    fn hard_pinned(&self) -> usize { self.cap / 2 }

    #[inline]
    fn bucket(&self, blk: u64) -> usize {
        (blk.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> self.hash_shift) as usize
    }

    #[inline]
    fn data(&self, i: usize) -> &[u8; BLOCK_SIZE] {
        let o = (i % CACHE_CHUNK) * BLOCK_SIZE;
        unsafe { &*(self.chunks[i / CACHE_CHUNK][o..o + BLOCK_SIZE].as_ptr() as *const [u8; BLOCK_SIZE]) }
    }

    #[inline]
    fn data_mut(&mut self, i: usize) -> &mut [u8; BLOCK_SIZE] {
        let o = (i % CACHE_CHUNK) * BLOCK_SIZE;
        unsafe { &mut *(self.chunks[i / CACHE_CHUNK][o..o + BLOCK_SIZE].as_mut_ptr() as *mut [u8; BLOCK_SIZE]) }
    }

    fn find(&self, blk: u64) -> Option<usize> {
        let mut i = self.buckets[self.bucket(blk)];
        while i != NIL {
            let m = &self.meta[i as usize];
            if m.blk_no == blk { return Some(i as usize); }
            i = m.hnext;
        }
        None
    }

    fn hash_insert(&mut self, i: usize) {
        let b = self.bucket(self.meta[i].blk_no);
        self.meta[i].hnext = self.buckets[b];
        self.buckets[b] = i as u32;
    }

    fn hash_remove(&mut self, i: usize) {
        let b = self.bucket(self.meta[i].blk_no);
        let mut p = self.buckets[b];
        if p == i as u32 {
            self.buckets[b] = self.meta[i].hnext;
            return;
        }
        while p != NIL {
            let n = self.meta[p as usize].hnext;
            if n == i as u32 {
                self.meta[p as usize].hnext = self.meta[i].hnext;
                return;
            }
            p = n;
        }
    }

    fn lru_unlink(&mut self, i: usize) {
        let (p, n) = (self.meta[i].prev, self.meta[i].next);
        if p != NIL { self.meta[p as usize].next = n; } else { self.head = n; }
        if n != NIL { self.meta[n as usize].prev = p; } else { self.tail = p; }
        self.meta[i].prev = NIL;
        self.meta[i].next = NIL;
    }

    fn lru_push_front(&mut self, i: usize) {
        self.meta[i].prev = NIL;
        self.meta[i].next = self.head;
        if self.head != NIL { self.meta[self.head as usize].prev = i as u32; } else { self.tail = i as u32; }
        self.head = i as u32;
    }

    fn lru_push_back(&mut self, i: usize) {
        self.meta[i].next = NIL;
        self.meta[i].prev = self.tail;
        if self.tail != NIL { self.meta[self.tail as usize].next = i as u32; } else { self.head = i as u32; }
        self.tail = i as u32;
    }

    #[inline]
    fn pinned(&self, i: usize) -> bool { self.meta[i].dirty || self.meta[i].wb }

    /// Change a slot's dirty/wb flags, keeping the counters and the LRU list
    /// (exactly the clean, unpinned slots) in step.
    fn set_flags(&mut self, i: usize, dirty: bool, wb: bool) {
        let was = self.pinned(i);
        if self.meta[i].dirty != dirty {
            if dirty { self.ndirty += 1; } else { self.ndirty -= 1; }
        }
        self.meta[i].dirty = dirty;
        self.meta[i].wb = wb;
        let now = dirty || wb;
        if was && !now {
            self.npinned -= 1;
            self.lru_push_front(i);
        } else if !was && now {
            self.npinned += 1;
            self.lru_unlink(i);
        }
    }

    /// Mark slot `i` most recently used.
    #[inline]
    fn touch(&mut self, i: usize) {
        if !self.pinned(i) && self.head != i as u32 {
            self.lru_unlink(i);
            self.lru_push_front(i);
        }
    }

    /// Add one 1 MiB chunk of never-used slots at the LRU tail. Fallible:
    /// an allocation failure leaves the cache as it was.
    fn grow(&mut self) -> bool {
        let bytes = CACHE_CHUNK * BLOCK_SIZE;
        let mut v: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        if v.try_reserve_exact(bytes).is_err() { return false; }
        if self.meta.try_reserve(CACHE_CHUNK).is_err() { return false; }
        if self.chunks.try_reserve(1).is_err() { return false; }
        v.resize(bytes, 0);
        self.chunks.push(v.into_boxed_slice());
        for _ in 0..CACHE_CHUNK {
            let i = self.meta.len();
            self.meta.push(SlotMeta { blk_no: NULL_BLK, dirty: false, wb: false, hnext: NIL, prev: NIL, next: NIL, uep: 0 });
            self.lru_push_back(i);
        }
        true
    }

    /// A slot for a block not in the cache, detached from its old block (if
    /// any) and still on the LRU list. Never writes in the normal case: it
    /// takes a never-used slot (growing up to the cap first), else the least
    /// recently used clean slot.
    fn take_slot(&mut self, dev: usize) -> usize {
        let tail_used = self.tail == NIL || self.meta[self.tail as usize].blk_no != NULL_BLK;
        if tail_used && self.meta.len() < self.cap { let _ = self.grow(); }
        if self.tail == NIL && self.meta.len() < self.cap * 2 && self.grow() {
            if self.exhausted == 0 {
                dbg_str("[F2FS] block cache: every slot is dirty or in write-back; growing past the cap\n");
            }
            self.exhausted += 1;
        }
        if self.tail != NIL {
            let i = self.tail as usize;
            if self.meta[i].blk_no != NULL_BLK {
                self.hash_remove(i);
                self.meta[i].blk_no = NULL_BLK;
            }
            self.meta[i].uep = 0;
            return i;
        }
        // Backstop: nothing evictable and the cache may not grow. Write one
        // pinned block back in place, under the lock (the pre-f2fsperf
        // eviction). Logged: the writer throttle should make this unreachable.
        // The block written may be one this request changed, which an undo
        // could no longer take back from the device: the request leaves
        // no-I/O mode first (no miss can have happened yet: a doomed request
        // never installs a block), so it will not be undone.
        self.probe_abandon();
        self.exhausted += 1;
        if self.exhausted % 1024 == 1 {
            dbg_str("[F2FS] block cache exhausted: synchronous write-back under the lock\n");
        }
        let i = match (0..self.meta.len()).find(|&i| self.meta[i].dirty && !self.meta[i].wb) {
            Some(i) => i,
            None => { wait_wb_idle(); 0 }
        };
        let blk = self.meta[i].blk_no;
        io_write_block(dev, blk, self.data(i));
        self.set_flags(i, false, false);
        self.hash_remove(i);
        self.meta[i].blk_no = NULL_BLK;
        self.meta[i].uep = 0;
        i
    }

    /// Give the detached slot `i` to block `blk`.
    fn install(&mut self, i: usize, blk: u64) {
        self.meta[i].blk_no = blk;
        self.hash_insert(i);
        self.touch(i);
        let g = Self::gen_idx(blk);
        self.gens[g] = self.gens[g].wrapping_add(1);
    }

    /// Load `blk` into the cache (clean) if absent; its slot. In a no-I/O
    /// request a miss is recorded instead (None: the request is doomed).
    fn load(&mut self, dev: usize, blk: u64) -> Option<usize> {
        if let Some(i) = self.find(blk) {
            self.touch(i);
            return Some(i);
        }
        if self.probe != PROBE_OFF {
            self.note_miss(blk, 1);
            return None;
        }
        let i = self.take_slot(dev);
        if !io_read_block(IO_READ_MISS, dev, blk, self.data_mut(i)) {
            self.data_mut(i).fill(0);
        }
        self.install(i, blk);
        Some(i)
    }

    fn read(&mut self, dev: usize, blk: u64) -> &[u8; BLOCK_SIZE] {
        match self.load(dev, blk) {
            Some(i) => self.data(i),
            None => &*self.scratch_r,
        }
    }

    fn write(&mut self, dev: usize, blk: u64, src: &[u8; BLOCK_SIZE]) {
        if self.probe == PROBE_MISSED { return; }
        let i = match self.find(blk) {
            Some(i) => { self.log_slot(i, false); i }
            None => {
                let i = self.take_slot(dev);
                self.install(i, blk);
                self.log_slot(i, true);
                i
            }
        };
        self.data_mut(i).copy_from_slice(src);
        let wb = self.meta[i].wb;
        self.set_flags(i, true, wb);
        self.dirtied += 1;
    }

    /// Write every dirty (and in-write-back) block back, in block order, one
    /// device request per run of adjacent blocks. Used by the synchronous,
    /// locked checkpoint (unmount, remount-ro, shutdown, no-sleep fallback).
    fn flush_all(&mut self, dev: usize) {
        // An unlocked checkpoint may still be writing an older copy of some
        // of these blocks: let it finish first, so that copy cannot land on
        // top of what is written here (lane loginhang). Slots it pinned (`wb`)
        // are written again too — their content is the snapshot or newer.
        wait_wb_idle();
        let mut idx: alloc::vec::Vec<usize> =
            (0..self.meta.len()).filter(|&i| self.pinned(i)).collect();
        if idx.is_empty() { return; }
        idx.sort_unstable_by_key(|&i| self.meta[i].blk_no);
        ACT_WB_TOTAL.store(idx.len() as u32, atomic::Ordering::Relaxed);
        ACT_WB_DONE.store(0, atomic::Ordering::Relaxed);
        let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let mut k = 0;
        while k < idx.len() {
            let mut n = 1;
            while k + n < idx.len() && n < MAX_RUN
                && self.meta[idx[k + n]].blk_no == self.meta[idx[k]].blk_no + n as u64
            {
                n += 1;
            }
            let start = self.meta[idx[k]].blk_no;
            if n == 1 {
                io_write_block(dev, start, self.data(idx[k]));
            } else {
                buf.clear();
                for j in 0..n { buf.extend_from_slice(self.data(idx[k + j])); }
                if !io_write_blocks(dev, start, &buf) {
                    // Device refused the multi-block request: fall back to
                    // single blocks so each one gets its own attempt.
                    for j in 0..n {
                        io_write_block(dev, start + j as u64, self.data(idx[k + j]));
                    }
                }
            }
            for j in 0..n { self.set_flags(idx[k + j], false, false); }
            k += n;
            ACT_WB_DONE.store(k as u32, atomic::Ordering::Relaxed);
        }
    }

    /// The cached block `blk` for an in-place change that does NOT dirty it
    /// (lazytime): the change is visible to every later read through the
    /// cache and reaches the medium only if the block is written back for
    /// another reason; an eviction drops it.
    fn get_clean_mut(&mut self, dev: usize, blk: u64) -> &mut [u8; BLOCK_SIZE] {
        if self.probe == PROBE_MISSED { return self.scratch_mut(); }
        let i = match self.load(dev, blk) { Some(i) => i, None => return self.scratch_mut() };
        self.log_slot(i, false);
        self.data_mut(i)
    }

    fn get_mut(&mut self, dev: usize, blk: u64) -> &mut [u8; BLOCK_SIZE] {
        if self.probe == PROBE_MISSED { return self.scratch_mut(); }
        let i = match self.load(dev, blk) { Some(i) => i, None => return self.scratch_mut() };
        self.log_slot(i, false);
        let wb = self.meta[i].wb;
        self.set_flags(i, true, wb);
        self.dirtied += 1;
        self.data_mut(i)
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
    /// `ckpt_flags` of the pack this was parsed from.
    flags:            u32,
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
            flags:           r32(blk, CP_CKPT_FLAGS),
        };
        (ver, cp)
    }
}

// ── Open file table ───────────────────────────────────────────────────────────

/// Open-file slots per mount. This pool is **per-mount and system-wide**, not
/// per-process: every process on a volume draws from the same array, and a
/// directory held open by `getdents64` occupies a slot exactly like a file
/// does. Everything on LeandrOS lives on the one f2fs root mount (the initrd
/// contributes three entries), so in practice this single number is the
/// whole system's open-file budget.
///
/// At 32 it was far too small. Ten distinct processes took errno 24 inside one
/// 3 s window at session start — concurrent config reads, fontdb directory
/// scans and icon-theme walks — and the exhaustion is not merely transient in
/// its effects: `freedesktop-icons` builds its theme map in a `LazyLock`,
/// initialises it exactly once, and `continue`s past a failed `read_dir`, so a
/// single EMFILE at the wrong microsecond leaves that process with an empty
/// theme map for the rest of its life. That is why cosmic-applet-tiling laid
/// out at full size and painted nothing.
///
/// Cost of 32 -> 256, measured rather than estimated. `size_of::<OpenFile>()`
/// is 216 B (dominated by `path: [u8; MAX_OPEN_PATH]`) and, at 256 slots,
/// `size_of::<Option<MountState>>()` is 71,872 B against 23,617 B at 32.
///
/// What made 32 -> 256 dangerous was never those bytes in the image, though it
/// looked that way for a day (TODO.md item 15). This constant sizes
/// `MountState`, `MountState` was assembled as a *stack temporary* before being
/// moved into the table, and a 71,872 B struct compiled to a 155,880 B frame on
/// what was then a 64 KiB kernel stack — so raising the constant silently
/// overran the stack by ~90 KB and corrupted the allocation underneath.
/// `F2FS_MOUNTS` now holds `Option<Box<MountState>>` and `mount()` builds the
/// value in place, so the constant costs heap at mount time and neither image
/// bytes nor stack. Raising it again is cheap.
///
/// Neither `MAX_MOUNTS` nor `MAX_OPEN_PATH` was cut to fund it. Shrinking
/// `MAX_OPEN_PATH` to 128 would silently stop `/proc/self/fd/N` resolving
/// exactly the long `/usr/share/icons/...` paths this bug is about, and halving
/// `MAX_MOUNTS` would trade a limit measured too small for one never tested.
const MAX_OPEN_FILES: usize = 256;

/// Longest absolute path remembered per open file, for `VFS_FD_PATH`
/// (`readlink("/proc/self/fd/N")`). Sized to fit any path the kernel's
/// `KPATH_MAX`-bounded resolver can hand us; longer ones are simply not
/// recovered rather than truncated into a lie.
const MAX_OPEN_PATH: usize = 192;

#[derive(Clone, Copy)]

struct OpenFile {
    inode:    u32,
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
        Self { inode: 0, writable: false, in_use: false,
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
    /// `monotonic_ns` of the last checkpoint.
    last_cp_ns:   u64,
    /// `monotonic_ns` of the first mutating request since the last
    /// checkpoint (valid while `dirty_writes > 0`): the background write-back
    /// commits once the oldest uncommitted change is `CP_INTERVAL_NS` old.
    first_dirty_ns: u64,
    /// Allocation state written by the last checkpoint (`cp_pos`).
    last_cp_pos:  [u32; 6],
    /// Sum of every main-area segment's SIT vblocks, as the cache holds them:
    /// computed once at mount (`sit_count_valid_blocks`) and adjusted by
    /// `sit_mark_block_used` / `sit_mark_block_free`, the only writers of a
    /// vblocks field. `statfs` reads it instead of re-summing the whole SIT.
    valid_blocks: u64,
    open_files:   [OpenFile; MAX_OPEN_FILES],
    cache:        BlockCache,
    /// Credentials of the process whose request is being served. Set by
    /// `dispatch_msg` on entry (the server is single-threaded and IPC is
    /// synchronous, so one `Cred` per mount is exact) and read by the path walk
    /// so every directory component can be checked for search permission
    /// without threading credentials through fifteen signatures.
    cred:         xattr::Cred,
    /// Why the last `resolve_path_ex` returned 0 (negative errno, 0 = ENOENT).
    /// Read through `take_walk_err`; see `resolve_path_r`.
    walk_err:     i32,
    /// Mounted (or remounted) read-only: every mutating request answers
    /// EROFS, reads do not touch atime, and no checkpoint is written — so the
    /// clean-unmount checkpoint taken when the flag was set stays the newest
    /// one until the volume is remounted read-write.
    read_only:    bool,
    /// Open-file slots the current no-I/O request changed, with their old
    /// contents (`of_log`; a request touches one slot, rarely two).
    of_undo:      [(usize, OpenFile); OF_UNDO_MAX],
    of_undo_n:    usize,
    /// Per-file fsync tracking (lane fsyncbatch): for each inode (hashed,
    /// `INO_MOD_SLOTS`; a collision only makes an fsync wait for a
    /// checkpoint it did not need), the number of the first checkpoint that
    /// contains its latest change (`cp_seq`'s `started + 1` when the change
    /// was made). An fsync of an inode whose number has completed has
    /// nothing to commit.
    ino_mod:      alloc::vec::Vec<u64>,
    /// The same for changes not attributed to one inode (namespace
    /// operations, path-based requests, anything `tracked` caught): every
    /// fsync waits for these.
    mod_global:   u64,
    /// The inode the current request changes, when the request names it by
    /// descriptor (set by `dispatch_locked`; 0 = unknown: changes count as
    /// `mod_global`).
    req_ino:      u32,
    /// The current request recorded its change (`maybe_flush`).
    req_marked:   bool,
}

/// Slots of `MountState::ino_mod` (32 KiB per mount).
const INO_MOD_SLOTS: usize = 4096;

#[inline]
fn ino_mod_idx(ino: u32) -> usize {
    ((ino as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 52) as usize % INO_MOD_SLOTS
}

/// The checkpoint that will contain a change made now (call under
/// F2FS_MOUNTS, which phase 1 of every checkpoint holds when it numbers
/// itself and takes its snapshot).
fn next_cp_seq(port: u32) -> u64 {
    cp_seq(port).map_or(u64::MAX, |s| s.started.load(atomic::Ordering::SeqCst) + 1)
}

/// The current request changed the volume: record which checkpoint must
/// complete before an fsync of the file it changed (or of anything, if the
/// file is unknown) may return.
fn mark_changed(ms: &mut MountState) {
    let n = next_cp_seq(ms.port);
    if ms.req_ino != 0 {
        let i = ino_mod_idx(ms.req_ino);
        ms.ino_mod[i] = ms.ino_mod[i].max(n);
    } else {
        ms.mod_global = ms.mod_global.max(n);
    }
    ms.req_marked = true;
}

/// A change every fsync must wait for (namespace operations).
fn mark_changed_global(ms: &mut MountState) {
    ms.mod_global = ms.mod_global.max(next_cp_seq(ms.port));
    ms.req_marked = true;
}

/// Run one request on `ms` and make sure a change it made is recorded for
/// fsync: a request that changed the allocator, the counters or any cached
/// block without calling `maybe_flush` counts as a change to everything. A
/// doomed run's marks stay (they only ever make an fsync wait longer).
fn tracked<R>(ms: &mut MountState, f: &mut impl FnMut(&mut MountState) -> R) -> R {
    let before = (ms.dirty_writes, ms.cache.dirtied, cp_pos(ms));
    ms.req_marked = false;
    let r = f(ms);
    if !ms.req_marked && (ms.dirty_writes, ms.cache.dirtied, cp_pos(ms)) != before {
        mark_changed_global(ms);
    }
    ms.req_ino = 0;
    r
}

const OF_UNDO_MAX: usize = 4;

const MAX_MOUNTS: usize = 8;

/// The mount table holds *pointers*, not inline `MountState`s, and that is a
/// correctness requirement rather than a tidiness one.
///
/// Inline, an entry of this array was a whole `MountState`, so filling a slot
/// meant constructing 71,872 B — and rustc builds that on the **stack** before
/// moving it in. `f2fs::mount` compiled to a 155,880 B frame against a 64 KiB
/// kernel stack with no guard page, and **stopped the x86_64 Linux box booting
/// on both architectures**: init faulted on its own stack during the very mount
/// this table serves, because the mount had already overwritten ~90 KB of the
/// buddy allocation below its stack. It looked host-specific only because the
/// Mac happened to survive the same corruption. Full account, and the four
/// experiments that separated frame size from image size, in TODO.md item 15.
///
/// Holding pointers is what breaks that link: a slot is one word, so no code
/// path constructs a `MountState` anywhere except in place. It is also cheaper
/// twice over — `Option<Box<..>>` is null-pointer-optimised, so all-`None` is
/// all-zero and the static is 64 B in `.bss` instead of 574,976 B in `.data`,
/// and the per-mount cost is now paid only by volumes actually mounted, two of
/// eight in a normal boot.
///
/// `scripts/check-stack-frames.py` fails the build if any frame gets near the
/// stack again; it is not left to whoever next edits `mount()` to remember.
///
/// The lock is a `sched::iolock::IoMutex`: a SLEEPING FIFO mutex (lane fsio).
/// It used to be held across block I/O — a read on the PC's USB stick takes
/// milliseconds per command, far longer when it queues behind a write — and
/// its waiters park instead of spinning, so the CPUs stay available to every
/// task that is not waiting for the filesystem (lane nvstall's ticket
/// spinlock kept up to three of four CPUs spinning with preemption disabled:
/// boot7, 40458 waits, 18.9 s max hold, a desktop that took minutes and a VT2
/// shell that echoed a second late). Since lane nostall no request reads or
/// writes the device with it held in normal operation: file data is read
/// with the lock dropped (`read_unlocked`), metadata misses are fetched with
/// it dropped and the request rerun (`with_mount`), and checkpoints write
/// unlocked (`checkpoint_unlocked`). Callers must hold no spinlock (the survey of every entry point is
/// in artifacts/notes/lane-fsio-2026-10-09.md); a caller that cannot sleep
/// degrades to a spin with interrupt windows and `[KSYNC]` names it once.
static F2FS_MOUNTS: sched::iolock::IoMutex<[Option<Box<MountState>>; MAX_MOUNTS]> =
    sched::iolock::IoMutex::new(sched::lockwatch::L_F2FS, "F2FS_MOUNTS", [const { None }; MAX_MOUNTS]);

type MountsInner = sched::iolock::IoGuard<'static, [Option<Box<MountState>>; MAX_MOUNTS]>;

/// The mount-table guard: `IoGuard` plus what the hold spent on device I/O.
/// On release, a hold of `LONG_HOLD_NS` or more is followed (after the lock
/// is free, after iolock's own `[LOCKHOLD]` line) by one `[F2FS] long hold`
/// line naming the request and splitting the time into device I/O by kind
/// and, of that, the wait for the disk lock (lane nostall).
struct MountsGuard { g: Option<MountsInner> }

impl core::ops::Deref for MountsGuard {
    type Target = [Option<Box<MountState>>; MAX_MOUNTS];
    #[inline]
    fn deref(&self) -> &Self::Target { self.g.as_ref().unwrap() }
}
impl core::ops::DerefMut for MountsGuard {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target { self.g.as_mut().unwrap() }
}

impl MountsGuard {
    fn new(g: MountsInner) -> Self {
        HOLD_T0.store(sched::monotonic_ns(), atomic::Ordering::Relaxed);
        HOLD_WAIT0.store(drivers::usb_storage::task_disk_wait_ns(sched::current_pid()), atomic::Ordering::Relaxed);
        for k in 0..IO_KINDS {
            HOLD_IO_NS[k].store(0, atomic::Ordering::Relaxed);
            HOLD_IO_N[k].store(0, atomic::Ordering::Relaxed);
        }
        MountsGuard { g: Some(g) }
    }
}

impl Drop for MountsGuard {
    fn drop(&mut self) {
        let now = sched::monotonic_ns();
        let held = now.saturating_sub(HOLD_T0.load(atomic::Ordering::Relaxed));
        let mut long = held >= sched::iolock::LONG_HOLD_NS;
        // The first few holds of the boot that did device I/O are reported
        // whatever their length: after lane nostall that should be only the
        // mounts (and unmount/shutdown), so anything else names a path left.
        if !long && HOLD_IO_N.iter().any(|n| n.load(atomic::Ordering::Relaxed) != 0)
            && FIRST_IO_HOLDS_REPORTED.fetch_add(1, atomic::Ordering::Relaxed) < 6
        {
            long = true;
        }
        let (mut io_ns, mut io_n) = ([0u64; IO_KINDS], [0u32; IO_KINDS]);
        let mut wait = 0;
        let mut code = 0;
        let mut loc = 0;
        if long {
            loc = ACT_LOC.load(atomic::Ordering::Relaxed);
            for k in 0..IO_KINDS {
                io_ns[k] = HOLD_IO_NS[k].load(atomic::Ordering::Relaxed);
                io_n[k] = HOLD_IO_N[k].load(atomic::Ordering::Relaxed);
            }
            wait = drivers::usb_storage::task_disk_wait_ns(sched::current_pid())
                .saturating_sub(HOLD_WAIT0.load(atomic::Ordering::Relaxed));
            code = ACT_CODE.load(atomic::Ordering::Relaxed);
        }
        drop(self.g.take());
        if long { report_long_hold(now, held, code, loc, &io_ns, &io_n, wait); }
    }
}

// ── Device I/O done while holding F2FS_MOUNTS (lane nostall) ──────────────────
//
// Every block request this server makes goes through `io_*` below, which
// time it and, when the caller holds the mount-table lock, charge it to the
// hold by kind. Read only by the holder's own `MountsGuard` drop.
const IO_KINDS: usize = 4;
const IO_READ_MISS: usize = 0;
const IO_READ_DIRECT: usize = 1;
const IO_WRITE: usize = 2;
const IO_FLUSH: usize = 3;
const IO_KIND_NAMES: [&str; IO_KINDS] = ["read-miss", "data-read", "write", "flush"];
static HOLD_T0: atomic::AtomicU64 = atomic::AtomicU64::new(0);
static HOLD_WAIT0: atomic::AtomicU64 = atomic::AtomicU64::new(0);
static HOLD_IO_NS: [atomic::AtomicU64; IO_KINDS] = [const { atomic::AtomicU64::new(0) }; IO_KINDS];
static HOLD_IO_N: [atomic::AtomicU32; IO_KINDS] = [const { atomic::AtomicU32::new(0) }; IO_KINDS];
/// Block requests issued with the lock held since the last summary window.
static LOCKED_IO_COUNT: atomic::AtomicU32 = atomic::AtomicU32::new(0);

fn io_charge(kind: usize, t0: u64) {
    let pid = sched::current_pid();
    if pid == 0 || !F2FS_MOUNTS.is_locked() || F2FS_MOUNTS.owner_pid() != pid { return; }
    HOLD_IO_NS[kind].fetch_add(sched::monotonic_ns().saturating_sub(t0), atomic::Ordering::Relaxed);
    HOLD_IO_N[kind].fetch_add(1, atomic::Ordering::Relaxed);
    LOCKED_IO_COUNT.fetch_add(1, atomic::Ordering::Relaxed);
}

fn io_read_block(kind: usize, dev: usize, blk: u64, buf: &mut [u8; BLOCK_SIZE]) -> bool {
    let t0 = sched::monotonic_ns();
    let r = virtio_blk::read_block(dev, blk, buf);
    io_charge(kind, t0);
    r
}

fn io_read_blocks(kind: usize, dev: usize, blk: u64, buf: &mut [u8]) -> bool {
    let t0 = sched::monotonic_ns();
    let r = virtio_blk::read_blocks(dev, blk, buf);
    io_charge(kind, t0);
    r
}

fn io_write_block(dev: usize, blk: u64, buf: &[u8; BLOCK_SIZE]) -> bool {
    let t0 = sched::monotonic_ns();
    let r = virtio_blk::write_block(dev, blk, buf);
    io_charge(IO_WRITE, t0);
    r
}

fn io_write_blocks(dev: usize, blk: u64, buf: &[u8]) -> bool {
    let t0 = sched::monotonic_ns();
    let r = virtio_blk::write_blocks(dev, blk, buf);
    io_charge(IO_WRITE, t0);
    r
}

fn io_flush(dev: usize) -> bool {
    let t0 = sched::monotonic_ns();
    let r = virtio_blk::flush(dev);
    io_charge(IO_FLUSH, t0);
    r
}

static LAST_LONG_HOLD_REPORT: atomic::AtomicU64 = atomic::AtomicU64::new(0);
static FIRST_IO_HOLDS_REPORTED: atomic::AtomicU32 = atomic::AtomicU32::new(0);

/// `[F2FS] long hold 1291 ms (stat): I/O under the lock 1285 ms: read-miss
/// 2x 1285 ms; disk-lock wait 1270 ms` — printed with the lock released.
fn report_long_hold(now: u64, held: u64, code: u64, loc: usize, io_ns: &[u64; IO_KINDS], io_n: &[u32; IO_KINDS], wait: u64) {
    let last = LAST_LONG_HOLD_REPORT.load(atomic::Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < 1_000_000_000 { return; }
    LAST_LONG_HOLD_REPORT.store(now, atomic::Ordering::Relaxed);
    let total: u64 = io_ns.iter().sum();
    dbg_str("[F2FS] long hold ");
    dbg_dec((held / 1_000_000) as usize);
    dbg_str(" ms (");
    dbg_str(act_name(code));
    if code < ACT_LOCKED && code != 0 {
        dbg_str(" tag ");
        dbg_dec(code as usize);
    }
    if loc != 0 {
        let l = unsafe { &*(loc as *const core::panic::Location<'static>) };
        dbg_str(" at ");
        dbg_str(l.file());
        dbg_str(":");
        dbg_dec(l.line() as usize);
    }
    dbg_str("): I/O under the lock ");
    dbg_dec((total / 1_000_000) as usize);
    dbg_str(" ms:");
    for k in 0..IO_KINDS {
        if io_n[k] == 0 { continue; }
        dbg_str(" ");
        dbg_str(IO_KIND_NAMES[k]);
        dbg_str(" ");
        dbg_dec(io_n[k] as usize);
        dbg_str("x ");
        dbg_dec((io_ns[k] / 1_000_000) as usize);
        dbg_str(" ms");
    }
    dbg_str("; disk-lock wait ");
    dbg_dec((wait / 1_000_000) as usize);
    dbg_str(" ms\n");
}

/// Open-file slots whose close arrived from a context that may not sleep
/// while the mount table was busy (an address space torn down in the
/// scheduler loop, munmap under the address-space lock): applied by the next
/// `lock_mounts`.
static PENDING_CLOSE: Mutex<alloc::vec::Vec<(u32, u64)>> = Mutex::new(alloc::vec::Vec::new());

/// Take the mount-table lock (sleeping) and apply deferred closes.
#[track_caller]
fn lock_mounts() -> MountsGuard {
    let loc = core::panic::Location::caller();
    let mut g = MountsGuard::new(F2FS_MOUNTS.lock());
    act_set(ACT_LOCKED, loc);
    apply_pending_closes(&mut g);
    g
}

// ── What the lock holder is doing (hung-task reports, lane loginhang) ─────────
//
// Written only by the F2FS_MOUNTS holder, read lock-free by the kernel's
// hung-task detector, which must never take the lock it is reporting on.

/// Generic: "holds the lock, entered at <file:line>".
const ACT_LOCKED: u64 = 0x1_0000;
/// A checkpoint: write-back of the dirty cache, then the commit block.
const ACT_CHECKPOINT: u64 = 0x1_0001;

static ACT_CODE: atomic::AtomicU64 = atomic::AtomicU64::new(0);
static ACT_LOC: atomic::AtomicUsize = atomic::AtomicUsize::new(0);
static ACT_T0: atomic::AtomicU64 = atomic::AtomicU64::new(0);
static ACT_WB_TOTAL: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static ACT_WB_DONE: atomic::AtomicU32 = atomic::AtomicU32::new(0);

fn act_set(code: u64, loc: &'static core::panic::Location<'static>) {
    ACT_CODE.store(code, atomic::Ordering::Relaxed);
    ACT_LOC.store(loc as *const _ as usize, atomic::Ordering::Relaxed);
    ACT_T0.store(sched::monotonic_ns(), atomic::Ordering::Relaxed);
    ACT_WB_TOTAL.store(0, atomic::Ordering::Relaxed);
    ACT_WB_DONE.store(0, atomic::Ordering::Relaxed);
}

#[track_caller]
fn act_code(code: u64) { act_set(code, core::panic::Location::caller()); }

fn act_name(code: u64) -> &'static str {
    match code {
        ACT_LOCKED => "holding the lock",
        ACT_CHECKPOINT => "checkpoint write-back",
        VFS_OPEN => "open", VFS_READ => "read", VFS_WRITE => "write", VFS_CLOSE => "close",
        VFS_STAT | VFS_LSTAT | VFS_FSTAT => "stat", VFS_GETDENTS64 => "getdents",
        VFS_UNLINK => "unlink", VFS_MKDIR => "mkdir", VFS_RMDIR => "rmdir",
        VFS_RENAME => "rename", VFS_FTRUNCATE => "ftruncate", VFS_FSYNC => "fsync",
        VFS_SYMLINK => "symlink", VFS_LINK => "link", VFS_READLINK => "readlink",
        VFS_FALLOCATE => "fallocate", VFS_ACCESS => "access",
        _ => "request",
    }
}

/// The filesystem lock as the hung-task detector reports it.
pub struct FsLockSnapshot {
    /// Holder pid (0 = free).
    pub owner_pid: u32,
    /// How long the current holder has held it.
    pub hold_ns: u64,
    pub waiters: usize,
    pub oldest_pid: u32,
    pub oldest_wait_ns: u64,
    /// e.g. "fsync (servers/f2fs/src/lib.rs:4123)".
    pub activity: alloc::string::String,
    pub op_age_ns: u64,
    pub wb_total: u32,
    pub wb_done: u32,
}

/// Lock-free snapshot of `F2FS_MOUNTS` (never takes it).
pub fn lock_snapshot() -> FsLockSnapshot {
    let (waiters, oldest_pid, oldest_wait_ns) = F2FS_MOUNTS.waiters();
    let owner_pid = if F2FS_MOUNTS.is_locked() { F2FS_MOUNTS.owner_pid() } else { 0 };
    let (_, hold_ns) = F2FS_MOUNTS.holder();
    let code = ACT_CODE.load(atomic::Ordering::Relaxed);
    let loc = ACT_LOC.load(atomic::Ordering::Relaxed);
    let mut activity = alloc::string::String::from(act_name(code));
    if code < ACT_LOCKED && code != 0 {
        let _ = core::fmt::Write::write_fmt(&mut activity, format_args!(" (VFS tag {:#x})", code));
    }
    if loc != 0 {
        let l = unsafe { &*(loc as *const core::panic::Location<'static>) };
        let _ = core::fmt::Write::write_fmt(&mut activity, format_args!(" at {}:{}", l.file(), l.line()));
    }
    FsLockSnapshot {
        owner_pid,
        hold_ns: if owner_pid != 0 { hold_ns } else { 0 },
        waiters, oldest_pid, oldest_wait_ns, activity,
        op_age_ns: sched::monotonic_ns().saturating_sub(ACT_T0.load(atomic::Ordering::Relaxed)),
        wb_total: ACT_WB_TOTAL.load(atomic::Ordering::Relaxed),
        wb_done: ACT_WB_DONE.load(atomic::Ordering::Relaxed),
    }
}

fn apply_pending_closes(g: &mut MountsGuard) {
    if PENDING_CLOSE.lock().is_empty() { return; }
    let pending = core::mem::take(&mut *PENDING_CLOSE.lock());
    for (port, file_id) in pending {
        for slot in g.iter_mut() {
            if let Some(ref mut ms) = slot {
                if ms.port == port && (file_id as usize) < MAX_OPEN_FILES {
                    ms.open_files[file_id as usize].in_use = false;
                    slot_closed(port, file_id as usize);
                }
            }
        }
    }
}

/// `(waits, total wait ns, max wait ns, max hold ns)` of the mount-table lock.
pub fn mount_lock_stats() -> sched::iolock::IoLockStats {
    F2FS_MOUNTS.stats()
}

/// Mount-table lock counters since the previous call (periodic summary).
pub fn mount_lock_window() -> sched::iolock::IoLockWindow {
    F2FS_MOUNTS.take_window()
}

// ── No device I/O under the lock (lane nostall, 2026-10-10) ───────────────────
//
// On the PC's USB stick (nvrm-boot13) F2FS_MOUNTS was still held 0.5-1.4 s at
// a time: a request whose metadata block (NAT, inode, dentry, node) was not
// cached read it with the lock held, the read queued on the disk behind a
// background checkpoint's write command (0.5-1.3 s each on that stick), and
// every other filesystem call in the system waited for the lock meanwhile.
//
// Now every request runs through `with_mount`: with the lock held and the
// block cache in no-I/O mode. A block the request needs that is not cached is
// NOT read; the cache records it as the request's miss and from then on the
// request is doomed — reads of missing blocks return zeros, changes go to a
// scratch block, open-file slots are left alone — and it runs to its end
// without touching anything real. Then every change it made before the miss
// is undone (cache slots from the undo log, open-file slots from `of_undo`,
// the allocator and counters from `ProbeSnap`), the lock is dropped, the
// missing block is read into the cache, the lock is re-taken and the request
// starts over. A request is thus atomic exactly as before: only its final,
// miss-free run takes effect, all of it under one hold of the lock.
//
// Correctness of the unlocked read: the block was not cached when the miss
// was recorded, so the device copy was current (dirty and in-write-back
// blocks are pinned in the cache, so a block absent from the cache has no
// write pending). It can only go stale if the block enters the cache, is
// written back and is dropped again while the read runs; every install bumps
// the block's generation (`BlockCache::gens`), and a read whose generation
// moved is discarded. Concurrent misses on the same block share one read
// (`FETCHING`).
//
// What still does I/O with the lock held: mount (the SIT scan, before anyone
// can wait), unmount / remount-ro / shutdown (the synchronous checkpoint), a
// context that may not sleep, a request that needs more than
// `NOIO_MAX_ROUNDS` reads, and the out-of-memory fallbacks. All of it is
// counted in the `[fslock]` line ("block requests under it").

/// Allocator state and counters a request may change, restored when it is
/// undone.
struct ProbeSnap {
    cp: CpInfo,
    dirty_writes: u32,
    last_cp_ns: u64,
    first_dirty_ns: u64,
    last_cp_pos: [u32; 6],
    valid_blocks: u64,
    walk_err: i32,
}

fn probe_begin(ms: &mut MountState) -> ProbeSnap {
    ms.cache.probe_start();
    ms.of_undo_n = 0;
    ProbeSnap {
        cp: ms.cp, dirty_writes: ms.dirty_writes, last_cp_ns: ms.last_cp_ns,
        first_dirty_ns: ms.first_dirty_ns, last_cp_pos: ms.last_cp_pos,
        valid_blocks: ms.valid_blocks, walk_err: ms.walk_err,
    }
}

/// End of a request run in no-I/O mode: None if it completed (keep it), or
/// the block run it missed, with everything it changed undone.
fn probe_end(ms: &mut MountState, snap: &ProbeSnap) -> Option<MissList> {
    let miss = ms.cache.probe_stop();
    if miss.is_some() {
        for k in (0..ms.of_undo_n).rev() {
            let (slot, f) = ms.of_undo[k];
            ms.open_files[slot] = f;
        }
        ms.cp = snap.cp;
        ms.dirty_writes = snap.dirty_writes;
        ms.last_cp_ns = snap.last_cp_ns;
        ms.first_dirty_ns = snap.first_dirty_ns;
        ms.last_cp_pos = snap.last_cp_pos;
        ms.valid_blocks = snap.valid_blocks;
        ms.walk_err = snap.walk_err;
    }
    ms.of_undo_n = 0;
    miss
}

/// Before a request changes open-file slot `slot`. False when the request
/// is doomed (a miss happened): skip the change, the result is discarded.
fn of_log(ms: &mut MountState, slot: usize) -> bool {
    match ms.cache.probe {
        PROBE_MISSED => false,
        PROBE_ON => {
            if ms.of_undo[..ms.of_undo_n].iter().any(|&(s, _)| s == slot) { return true; }
            if ms.of_undo_n == OF_UNDO_MAX {
                ms.cache.probe_abandon();
                return true;
            }
            ms.of_undo[ms.of_undo_n] = (slot, ms.open_files[slot]);
            ms.of_undo_n += 1;
            true
        }
        _ => true,
    }
}

/// Runs of one request before it is allowed to read with the lock held. A
/// cold path walk needs a few reads per component (NAT, inode, dentry), a
/// cold `getdents` of a big directory one per dentry block.
const NOIO_MAX_ROUNDS: u32 = 1024;

/// Unlocked miss reads, reads shared with a concurrent miss of the same
/// block, reads discarded as possibly stale, requests that hit
/// `NOIO_MAX_ROUNDS`, no-I/O mode abandoned (out of memory) — since the
/// last summary window; plus the slowest unlocked read (ns).
static NOIO_FETCHES: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static NOIO_SHARED: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static NOIO_STALE: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static NOIO_GAVE_UP: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static NOIO_ABANDONED: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static NOIO_FETCH_MAX_NS: atomic::AtomicU64 = atomic::AtomicU64::new(0);
/// Fetch commands issued in the same unlocked pass as an earlier one (a
/// round that used to be one more lock drop + device wait each; lane
/// fsyncbatch), since the last summary window.
static NOIO_BATCHED: atomic::AtomicU32 = atomic::AtomicU32::new(0);

/// `(extra commands batched into an earlier pass, extra misses recorded by
/// doomed runs)` since the previous call.
pub fn noio_batch_window() -> (u32, u32) {
    (NOIO_BATCHED.swap(0, atomic::Ordering::Relaxed), NOIO_MISS_EXTRA.swap(0, atomic::Ordering::Relaxed))
}
static NOIO_MISS_EXTRA: atomic::AtomicU32 = atomic::AtomicU32::new(0);

/// `(unlocked miss reads, shared, stale, gave up (incl. requests from a
/// context that may not sleep), abandoned, slowest ms)`
/// since the previous call (the `[fslock]` summary).
pub fn noio_window() -> (u32, u32, u32, u32, u32, u64) {
    (NOIO_FETCHES.swap(0, atomic::Ordering::Relaxed), NOIO_SHARED.swap(0, atomic::Ordering::Relaxed),
     NOIO_STALE.swap(0, atomic::Ordering::Relaxed),
     NOIO_GAVE_UP.swap(0, atomic::Ordering::Relaxed) + NOIO_ATOMIC.swap(0, atomic::Ordering::Relaxed),
     NOIO_ABANDONED.swap(0, atomic::Ordering::Relaxed),
     NOIO_FETCH_MAX_NS.swap(0, atomic::Ordering::Relaxed) / 1_000_000)
}

/// Blocks some task is reading unlocked for a miss: `(dev, blk)`.
const FETCH_SLOTS: usize = 16;
static FETCHING: Mutex<[(usize, u64); FETCH_SLOTS]> = Mutex::new([(usize::MAX, 0); FETCH_SLOTS]);

/// Claim the read of `blk`: Some(true) claimed, Some(false) not tracked
/// (table full; read anyway), None if another task is reading it.
fn fetch_claim(dev: usize, blk: u64) -> Option<bool> {
    let mut t = FETCHING.lock();
    if t.iter().any(|&e| e == (dev, blk)) { return None; }
    match t.iter_mut().find(|e| e.0 == usize::MAX) {
        Some(e) => {
            *e = (dev, blk);
            // Others wait on this claim: like a held sleeping lock, it keeps
            // a group kill from reaping this task before it is released.
            sched::ksync::must_finish_begin();
            Some(true)
        }
        None => Some(false),
    }
}

fn fetch_release(dev: usize, blk: u64) {
    let mut t = FETCHING.lock();
    if let Some(e) = t.iter_mut().find(|e| **e == (dev, blk)) { *e = (usize::MAX, 0); }
    drop(t);
    sched::ksync::must_finish_end();
}

fn fetch_busy(dev: usize, blk: u64) -> bool {
    FETCHING.lock().iter().any(|&e| e == (dev, blk))
}

/// Park for about `ns` (task context that may sleep).
fn nap(ns: u64) {
    let d = sched::monotonic_ns().saturating_add(ns);
    sched::block_on_poll_prepare_masked(d, sched::POLL_MASK_TIMED_ONLY);
    if sched::monotonic_ns() >= d {
        sched::block_on_poll_cancel();
    } else {
        sched::block_on_poll_commit();
    }
}

/// One device command of a fetch pass: `n` blocks at `blk`.
#[derive(Clone, Copy)]
struct FetchRange { blk: u64, n: usize }

/// A doomed run's misses as device commands (lane fsyncbatch): sorted, with
/// overlapping misses and misses at most `FETCH_GAP` blocks apart joined,
/// each command at most `FETCH_MAX` blocks.
fn plan_fetch(list: &MissList) -> ([FetchRange; MISS_BATCH], usize) {
    let (mut m, n) = (list.0, list.1.min(MISS_BATCH));
    m[..n].sort_unstable_by_key(|e| e.0);
    let mut out = [FetchRange { blk: 0, n: 0 }; MISS_BATCH];
    let mut k = 0;
    for &(b, c) in &m[..n] {
        let e = b + c.max(1) as u64;
        if k > 0 {
            let last = &mut out[k - 1];
            let lend = last.blk + last.n as u64;
            let nend = e.max(lend);
            if b <= lend + FETCH_GAP && nend - last.blk <= FETCH_MAX as u64 {
                last.n = (nend - last.blk) as usize;
                continue;
            }
        }
        out[k] = FetchRange { blk: b, n: c.max(1) as usize };
        k += 1;
    }
    (out, k)
}

/// Read one fetch command into a fresh buffer (zeros for a failed read, as
/// a locked miss caches). None if out of memory.
fn fetch_read(dev: usize, r: FetchRange) -> Option<alloc::vec::Vec<u8>> {
    let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    if buf.try_reserve_exact(r.n * BLOCK_SIZE).is_err() { return None; }
    buf.resize(r.n * BLOCK_SIZE, 0);
    let t0 = sched::monotonic_ns();
    let ok = if r.n == 1 {
        virtio_blk::read_block(dev, r.blk, (&mut buf[..BLOCK_SIZE]).try_into().unwrap())
    } else {
        virtio_blk::read_blocks(dev, r.blk, &mut buf)
    };
    if !ok { buf.fill(0); }
    NOIO_FETCHES.fetch_add(1, atomic::Ordering::Relaxed);
    NOIO_FETCH_MAX_NS.fetch_max(sched::monotonic_ns().saturating_sub(t0), atomic::Ordering::Relaxed);
    Some(buf)
}

/// The unlocked half of a miss round: read every command of `ranges` with
/// no filesystem lock held, one after the other, so the request waits for
/// the device once per round rather than once per missing block. A command
/// whose first block another task is already reading is not read twice:
/// after our own reads we wait for that reader (its blocks are then in the
/// cache, unless evicted again) and, if its claim outlives any plausible
/// read (only a task that never came back can leak one), read it ourselves.
/// Per command: the data and whether its read was claimed (release with
/// `fetch_release` after installing), or None (shared, or out of memory).
fn fetch_pass(dev: usize, ranges: &[FetchRange]) -> [Option<(alloc::vec::Vec<u8>, bool)>; MISS_BATCH] {
    let mut out: [Option<(alloc::vec::Vec<u8>, bool)>; MISS_BATCH] = [const { None }; MISS_BATCH];
    let mut claims = [None::<bool>; MISS_BATCH];
    for (i, r) in ranges.iter().enumerate() { claims[i] = fetch_claim(dev, r.blk); }
    for (i, r) in ranges.iter().enumerate() {
        let Some(claimed) = claims[i] else { continue };
        match fetch_read(dev, *r) {
            Some(buf) => out[i] = Some((buf, claimed)),
            None => if claimed { fetch_release(dev, r.blk); },
        }
    }
    for (i, r) in ranges.iter().enumerate() {
        if claims[i].is_some() { continue; }
        NOIO_SHARED.fetch_add(1, atomic::Ordering::Relaxed);
        let give_up = sched::monotonic_ns().saturating_add(10_000_000_000);
        while fetch_busy(dev, r.blk) && sched::monotonic_ns() < give_up { nap(500_000); }
        if fetch_busy(dev, r.blk) {
            if let Some(buf) = fetch_read(dev, *r) { out[i] = Some((buf, false)); }
        }
    }
    out
}

static NOIO_GAVE_UP_REPORTED: atomic::AtomicBool = atomic::AtomicBool::new(false);
/// Requests run in a context that may not sleep (so with reads under the
/// lock, as before lane nostall) since the last summary window.
static NOIO_ATOMIC: atomic::AtomicU32 = atomic::AtomicU32::new(0);

/// A request comes from a context that may not sleep: count it, and name
/// the first such context once (before the lock is taken).
fn note_atomic_request() {
    static REPORTED: atomic::AtomicBool = atomic::AtomicBool::new(false);
    NOIO_ATOMIC.fetch_add(1, atomic::Ordering::Relaxed);
    if !REPORTED.swap(true, atomic::Ordering::Relaxed) {
        dbg_str("[F2FS] a request from a context that may not sleep (");
        dbg_str(sched::irqctx::sleep_forbidden_reason().unwrap_or("pid 0"));
        dbg_str("): its cache misses are read with the lock held
");
    }
}

/// `f2fs.nostall=0` on the kernel command line turns the no-I/O request mode
/// off (metadata misses are read with F2FS_MOUNTS held, as before lane
/// nostall): a one-boot A/B on the PC (lane fwintegrity). Read once.
fn nostall_enabled() -> bool {
    static STATE: atomic::AtomicU8 = atomic::AtomicU8::new(0); // 0 unknown, 1 on, 2 off
    match STATE.load(atomic::Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = drivers::nvidia_probe::cmdline_opt("f2fs.nostall").map_or(true, |v| v != 0);
            STATE.store(if on { 1 } else { 2 }, atomic::Ordering::Relaxed);
            if !on { dbg_str("[F2FS] f2fs.nostall=0: metadata misses are read with the lock held\n"); }
            on
        }
    }
}

/// Run `f` on the mount behind `port` with F2FS_MOUNTS held and no device
/// I/O under it (see above); None if no f2fs volume is behind `port`. `f`
/// may run several times and must keep everything it changes inside the
/// `MountState` (the block cache, open-file slots via `of_log`, the
/// allocator); its result is used only from the run that completed.
fn with_mount<R>(port: u32, mut f: impl FnMut(&mut MountState) -> R) -> Option<R> {
    if !nostall_enabled() {
        // `f2fs.nostall=0`: metadata misses read with the lock held, as
        // before lane nostall (one run, no undo).
        let mut mounts = lock_mounts();
        let ms = mounts.iter_mut().flatten().find(|m| m.port == port)?;
        return Some(tracked(ms, &mut f));
    }
    let noio = sched::irqctx::can_sleep() && sched::current_pid() != 0;
    if !noio { note_atomic_request(); }
    let mut mounts = lock_mounts();
    let mut rounds = 0u32;
    loop {
        let ms = mounts.iter_mut().flatten().find(|m| m.port == port)?;
        if !noio || rounds >= NOIO_MAX_ROUNDS {
            if noio {
                NOIO_GAVE_UP.fetch_add(1, atomic::Ordering::Relaxed);
                if !NOIO_GAVE_UP_REPORTED.swap(true, atomic::Ordering::Relaxed) {
                    dbg_str("[F2FS] a request needed more than 1024 unlocked metadata reads; finishing it with reads under the lock\n");
                }
            }
            return Some(tracked(ms, &mut f));
        }
        let snap = probe_begin(ms);
        let r = tracked(ms, &mut f);
        let list = match probe_end(ms, &snap) {
            None => return Some(r),
            Some(m) => m,
        };
        drop(r);
        // Every miss of the doomed run, fetched in one unlocked pass (lane
        // fsyncbatch; one miss per round before). The install generation of
        // every block is taken now, with the lock held and the block absent.
        let dev = ms.dev;
        let (ranges, k) = plan_fetch(&list);
        let mut gens = [0u32; MISS_BATCH * FETCH_MAX];
        let mut g = 0;
        for r in &ranges[..k] {
            for j in 0..r.n { gens[g] = ms.cache.blk_gen(r.blk + j as u64); g += 1; }
        }
        if k > 1 { NOIO_BATCHED.fetch_add(k as u32 - 1, atomic::Ordering::Relaxed); }
        drop(mounts);
        let fetched = fetch_pass(dev, &ranges[..k]);
        mounts = lock_mounts();
        let ms = mounts.iter_mut().flatten().find(|m| m.port == port);
        let mut g = 0;
        let mut ms = ms;
        for (i, r) in ranges[..k].iter().enumerate() {
            if let Some((buf, claimed)) = &fetched[i] {
                if let Some(ms) = ms.as_deref_mut() {
                    for j in 0..r.n {
                        if !ms.cache.install_fetched(dev, r.blk + j as u64, &buf[j * BLOCK_SIZE..], gens[g + j]) {
                            NOIO_STALE.fetch_add(1, atomic::Ordering::Relaxed);
                        }
                    }
                }
                if *claimed { fetch_release(dev, r.blk); }
            }
            g += r.n;
        }
        rounds += 1;
    }
}

// ── File data generations (lock-free bulk reads) ──────────────────────────────
//
// `read_unlocked` maps a read to device block runs with the lock held, drops
// the lock for the device I/O, and re-takes it to check that nothing changed
// the file's block map meanwhile. Every change of a file's data or block map
// bumps its generation (`data_changed`: write, truncate, freeing the inode); a
// block can only be reused by another file after it was freed from this one,
// which bumps this one's generation. A mismatch redoes the read under the
// lock. Generations are hashed into a small table: a collision only costs a
// spurious redo.
const GEN_SLOTS: usize = 1024;
static DATA_GEN: [atomic::AtomicU32; GEN_SLOTS] = [const { atomic::AtomicU32::new(0) }; GEN_SLOTS];

fn gen_slot(port: u32, ino: u32) -> &'static atomic::AtomicU32 {
    let h = ((port as u64) << 32 | ino as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 54;
    &DATA_GEN[h as usize % GEN_SLOTS]
}

/// The data (or block map) of `ino` changed: redo any lock-free read in
/// flight. Called with the mount-table lock held. (The kernel page cache is
/// updated, not invalidated, by the request itself once it completes: see
/// "Page cache glue".)
fn data_changed(ms: &MountState, ino: u32) {
    gen_slot(ms.port, ino).fetch_add(1, atomic::Ordering::AcqRel);
}

// ── Page cache glue (lane pagecache) ──────────────────────────────────────────
//
// Regular-file data is read through the kernel's unified page cache
// (`mm::pagecache`, whose header has the model): read(2), pread(2) and every
// mapping fault go through `pc_read`, which copies from cached pages without
// this server's lock and fills misses — with readahead — by reading the file
// into fresh frames with `read_unlocked`. The cache is kept coherent from
// here: every data change is applied to the cached pages at the end of the
// request that makes it, under F2FS_MOUNTS (`pc_write_through`, the
// `pagecache::truncate` in `truncate_to`, `drop_file` when an inode is
// freed). Only regular files are cached; directories and symlinks are read
// as before.
//
// A file's position is no longer kept in its open-file slot but in
// `SLOT_FAST`, next to a per-slot sleeping lock (Linux's f_pos_lock): a read
// that hits the cache must not need F2FS_MOUNTS just to learn and advance the
// offset. Every request that uses or moves the offset (read, write, lseek,
// getdents) holds that slot's lock around the whole request, so offset
// updates stay atomic per descriptor; the lock is always taken before
// F2FS_MOUNTS, never under it.

/// Per open-file slot: the offset, the inode (0: slot not open), and the
/// lock that serialises offset users. Indexed like `CP_SEQ` (mount) and
/// `open_files` (slot).
struct SlotFast {
    lock: sched::ksync::KRawMutex,
    ino: atomic::AtomicU32,
    pos: atomic::AtomicU64,
}

impl SlotFast {
    const fn new() -> Self {
        SlotFast { lock: sched::ksync::KRawMutex::new(), ino: atomic::AtomicU32::new(0),
                   pos: atomic::AtomicU64::new(0) }
    }
}

static SLOT_FAST: [[SlotFast; MAX_OPEN_FILES]; MAX_MOUNTS] =
    [const { [const { SlotFast::new() }; MAX_OPEN_FILES] }; MAX_MOUNTS];

fn slot_fast(port: u32, slot: usize) -> Option<&'static SlotFast> {
    if slot >= MAX_OPEN_FILES { return None; }
    let m = CP_SEQ.iter().position(|s| s.port.load(atomic::Ordering::Acquire) == port)?;
    Some(&SLOT_FAST[m][slot])
}

/// Slot `slot` of the mount at `port` now names `ino`, at offset 0 (a
/// completed open). Called under F2FS_MOUNTS from the final round.
fn slot_opened(port: u32, slot: usize, ino: u32) {
    if let Some(s) = slot_fast(port, slot) {
        s.pos.store(0, atomic::Ordering::Relaxed);
        s.ino.store(ino, atomic::Ordering::Release);
    }
}

fn slot_closed(port: u32, slot: usize) {
    if let Some(s) = slot_fast(port, slot) { s.ino.store(0, atomic::Ordering::Release); }
}

/// RAII hold of a slot's offset lock.
struct SlotGuard(&'static SlotFast);
impl SlotGuard {
    fn take(port: u32, slot: usize) -> Option<SlotGuard> {
        let s = slot_fast(port, slot)?;
        s.lock.lock();
        Some(SlotGuard(s))
    }
    fn ino(&self) -> u32 { self.0.ino.load(atomic::Ordering::Acquire) }
    fn pos(&self) -> u64 { self.0.pos.load(atomic::Ordering::Relaxed) }
    fn set_pos(&self, p: u64) { self.0.pos.store(p, atomic::Ordering::Relaxed); }
}
impl Drop for SlotGuard {
    fn drop(&mut self) { self.0.lock.unlock(); }
}

/// `(mount port, inode)` back from a page-cache key.
fn key_port_ino(key: u64) -> (u32, u32) {
    (((key >> 32) as u32).wrapping_sub(1), key as u32)
}

/// Apply `pcache=` / `pcache.max_mb=` from the command line (first mount).
fn pc_configure() {
    static DONE: atomic::AtomicBool = atomic::AtomicBool::new(false);
    if DONE.swap(true, atomic::Ordering::Relaxed) { return; }
    let on = drivers::nvidia_probe::cmdline_opt("pcache").map_or(true, |v| v != 0);
    let cap = drivers::nvidia_probe::cmdline_opt("pcache.max_mb");
    mm::pagecache::configure(on, cap);
    dbg_str("[pcache] ");
    if on { dbg_str("on, cap "); } else { dbg_str("OFF (pcache=0): read(2) uncached, mapping-lifetime caching; cap "); }
    dbg_dec((mm::pagecache::cap_pages() * BLOCK_SIZE) >> 20);
    dbg_str(" MiB\n");
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PcMode {
    /// read(2)/pread(2): respects `pcache=0` and the cap (an uncached read
    /// when there is no room).
    Read,
    /// A mapping's fault: always cached (a shared mapping maps the frame),
    /// even over the cap.
    Fault,
}

/// Pages copied out per pin batch.
const PC_BATCH: usize = 16;
/// Largest readahead window (2 MiB): one fill, a few large commands on the
/// stick.
const PC_RA_MAX: usize = 512;
/// How long a relatime "no update needed" answer is trusted for a cached
/// file whose mtime/ctime did not change (atime is lazytime anyway).
const PC_ATIME_PERIOD_NS: u64 = 3_600_000_000_000;

/// Wait a little for another task's fill.
fn pc_wait() {
    mm::pagecache::COUNTERS.waits.fetch_add(1, atomic::Ordering::Relaxed);
    if sched::irqctx::can_sleep() && sched::current_pid() != 0 {
        nap(200_000);
    } else {
        let t = sched::monotonic_ns() + 50_000;
        while sched::monotonic_ns() < t { core::hint::spin_loop(); }
    }
}

/// Is `ino` a regular file, and how big? Teaches the cache its size (from a
/// completed round only: a doomed round's inode block may be a scratch copy).
fn pc_learn(port: u32, ino: u32, key: u64) -> Option<(bool, u64)> {
    with_mount(port, |ms| {
        act_code(VFS_READ);
        let addr = nat_lookup(ms, ino);
        if addr == 0 { return None; }
        let (mode, size) = {
            let iblk = ms.cache.read(ms.dev, addr as u64);
            (inode_mode(iblk), inode_size(iblk))
        };
        let reg = mode & S_IFMT == S_IFREG;
        if reg && !ms.cache.poisoned() { mm::pagecache::set_size(key, size); }
        Some((reg, size))
    }).flatten()
}

/// The read as it was before the cache: straight from the filesystem into
/// `dst` (device reads unlocked).
fn pc_uncached(port: u32, ino: u32, pos: u64, dst: *mut u8, len: usize) -> isize {
    read_unlocked(port, dst, len, &|_| Ok((ino, pos)), None)
}

/// `n` physically contiguous frames for a fill, or fewer when RAM is
/// fragmented (down to one). `(first frame, pages)`; pages 0 if none at all.
fn pc_alloc_run(n: usize) -> (usize, usize) {
    let mut order = 0usize;
    while (1usize << order) < n { order += 1; }
    loop {
        let got = if order == 0 { mm::buddy::alloc(0) } else { mm::buddy::try_alloc(order) };
        if let Some(base) = got {
            let pages = n.min(1usize << order);
            for i in pages..(1usize << order) { mm::buddy::free(base + i * BLOCK_SIZE, 0); }
            return (base, pages);
        }
        if order == 0 { return (0, 0); }
        order -= 1;
    }
}

enum FillErr {
    /// Cache full and nothing to evict: serve the rest uncached.
    NoRoom,
    /// The page is cached or being filled by someone else now, or the file
    /// left the cache: look again.
    Retry,
    Io(isize),
}

/// Fill a miss at page `pg` of a request with `req` pages left: readahead
/// window, placeholders, one read of the whole run into fresh frames,
/// publish. Ok(pages published).
fn pc_fill(port: u32, ino: u32, key: u64, pg: u64, req: usize, mode: PcMode) -> Result<usize, FillErr> {
    let want = mm::pagecache::ra_window(key, pg, req.min(PC_RA_MAX), PC_RA_MAX);
    if !mm::pagecache::make_room(want, mode == PcMode::Fault) {
        mm::pagecache::COUNTERS.bypass.fetch_add(1, atomic::Ordering::Relaxed);
        return Err(FillErr::NoRoom);
    }
    let (token, n) = match mm::pagecache::begin_fill(key, pg, want) {
        Some(t) => t,
        None => return Err(FillErr::Retry),
    };
    let (base, got_pages) = pc_alloc_run(n);
    if got_pages == 0 {
        mm::pagecache::abort_fill(key, token, pg, n);
        return Err(if mode == PcMode::Fault { FillErr::Io(-12) } else { FillErr::NoRoom });
    }
    if got_pages < n { mm::pagecache::abort_fill(key, token, pg + got_pages as u64, n - got_pages); }
    let n = got_pages;
    // Others wait on these placeholders: finish before a group kill reaps us.
    sched::ksync::must_finish_begin();
    let virt = mm::phys_to_virt(base);
    let got = read_unlocked(port, virt as *mut u8, n * BLOCK_SIZE,
                            &|_| Ok((ino, pg * BLOCK_SIZE as u64)), None);
    let mut frames: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
    if got < 0 || frames.try_reserve_exact(n).is_err() {
        mm::pagecache::abort_fill(key, token, pg, n);
        sched::ksync::must_finish_end();
        for i in 0..n { mm::buddy::free(base + i * BLOCK_SIZE, 0); }
        return Err(FillErr::Io(if got < 0 { got } else { -12 }));
    }
    let got = (got as usize).min(n * BLOCK_SIZE);
    // Past EOF the frame reads as zeros (the cache's invariant).
    unsafe { core::ptr::write_bytes((virt + got) as *mut u8, 0, n * BLOCK_SIZE - got); }
    mm::pagecache::clean_dcache(virt, n * BLOCK_SIZE);
    for i in 0..n { frames.push(base + i * BLOCK_SIZE); }
    let published = mm::pagecache::end_fill(key, token, pg, &frames, got);
    sched::ksync::must_finish_end();
    let c = &mm::pagecache::COUNTERS;
    c.misses.fetch_add(n.min(req), atomic::Ordering::Relaxed);
    c.readahead.fetch_add(n.saturating_sub(req), atomic::Ordering::Relaxed);
    if got == 0 { return Err(FillErr::Io(0)); } // end of file moved under us
    Ok(published)
}

/// Read `len` bytes at `pos` of regular file `ino` on the mount at `port`
/// into `dst` (a user or kernel pointer) through the page cache. Bytes read
/// (short only at end of file or on a fault), or a negative errno.
fn pc_read(port: u32, ino: u32, pos: u64, dst: *mut u8, len: usize, mode: PcMode) -> isize {
    if len == 0 { return 0; }
    if mode == PcMode::Read && !mm::pagecache::read_cache_enabled() {
        return pc_uncached(port, ino, pos, dst, len);
    }
    let key = page_cache_key(port, ino);
    let mut size = match mm::pagecache::file_size(key) {
        Some(s) => s,
        None => match pc_learn(port, ino, key) {
            Some((true, s)) => s,
            Some((false, _)) => return pc_uncached(port, ino, pos, dst, len),
            None => return -9,
        },
    };
    if pos >= size { return 0; }
    let mut end = pos.saturating_add(len as u64).min(size);
    let mut cur = pos;
    let mut frames = [0usize; PC_BATCH];
    let mut retries = 0u32;
    let ps = BLOCK_SIZE as u64;
    while cur < end {
        let pg = cur / ps;
        match mm::pagecache::pin_run(key, pg, &mut frames) {
            mm::pagecache::Run::Hit(n) => {
                retries = 0;
                let mut short = false;
                for (i, &phys) in frames[..n].iter().enumerate() {
                    let pstart = (pg + i as u64) * ps;
                    if pstart >= end { break; }
                    let lo = cur.max(pstart);
                    let hi = end.min(pstart + ps);
                    let src = (mm::phys_to_virt(phys) + (lo - pstart) as usize) as *const u8;
                    let d = unsafe { dst.add((lo - pos) as usize) };
                    let left = unsafe { sched::uaccess::copy_raw(d, src, (hi - lo) as usize) };
                    if left != 0 {
                        cur = hi - left as u64;
                        short = true;
                        break;
                    }
                    cur = hi;
                }
                mm::pagecache::unpin(&frames[..n]);
                if short {
                    return if cur > pos { (cur - pos) as isize } else { -14 }; // EFAULT
                }
            }
            mm::pagecache::Run::Busy => pc_wait(),
            mm::pagecache::Run::Miss => {
                let req = ((end - pg * ps + ps - 1) / ps) as usize;
                match pc_fill(port, ino, key, pg, req, mode) {
                    Ok(_) => {}
                    Err(FillErr::Retry) => {
                        retries += 1;
                        // The file may have shrunk (no page to fill there)
                        // or left the cache (its size must be learnt again).
                        match mm::pagecache::file_size(key) {
                            Some(s) => end = end.min(s.max(cur)),
                            None => match pc_learn(port, ino, key) {
                                Some((true, s)) => { size = s; end = end.min(size.max(cur)); }
                                _ => break,
                            },
                        }
                        if cur >= end {
                            break;
                        } else if retries > 64 {
                            // Keeps losing to writers or eviction: the rest
                            // straight from the filesystem.
                            let r = pc_uncached(port, ino, cur, unsafe { dst.add((cur - pos) as usize) },
                                                (end - cur) as usize);
                            return if r < 0 { if cur > pos { (cur - pos) as isize } else { r } }
                                   else { (cur - pos) as isize + r };
                        } else {
                            pc_wait();
                        }
                    }
                    Err(FillErr::NoRoom) => {
                        let r = pc_uncached(port, ino, cur, unsafe { dst.add((cur - pos) as usize) },
                                            (end - cur) as usize);
                        return if r < 0 { if cur > pos { (cur - pos) as isize } else { r } }
                               else { (cur - pos) as isize + r };
                    }
                    Err(FillErr::Io(0)) => break, // the file shrank under us
                    Err(FillErr::Io(e)) => {
                        return if cur > pos { (cur - pos) as isize } else { e };
                    }
                }
            }
        }
    }
    (cur - pos) as isize
}

/// relatime for a read served (partly) from the cache: ask the filesystem
/// only when the cache cannot vouch that no update is needed.
fn pc_atime(port: u32, ino: u32) {
    let key = page_cache_key(port, ino);
    let now = sched::monotonic_ns();
    if !mm::pagecache::atime_due(key, now, PC_ATIME_PERIOD_NS) { return; }
    let _ = with_mount(port, |ms| {
        touch_atime_relatime(ms, ino);
        if !ms.cache.poisoned() { mm::pagecache::atime_checked(key, now); }
    });
}

/// End of a write of `done` bytes at `pos` from `src` (final round, lock
/// held): the cached pages of the range get the new bytes, the gap a write
/// past EOF leaves in the old last page is zeroed, the size follows.
fn pc_write_through(port: u32, ino: u32, pos: u64, src: *const u8, done: usize,
                    old_size: u64, new_size: u64) {
    if done == 0 { return; }
    let key = page_cache_key(port, ino);
    if pos > old_size { mm::pagecache::extend(key, old_size, pos); }
    let mut pins: alloc::vec::Vec<mm::pagecache::Pinned> = alloc::vec::Vec::new();
    mm::pagecache::wt_pin(key, pos, done, &mut pins);
    let ps = BLOCK_SIZE as u64;
    for p in pins.iter() {
        let pstart = p.pgoff * ps;
        let lo = pos.max(pstart);
        let hi = (pos + done as u64).min(pstart + ps);
        if hi <= lo { continue; }
        let d = (mm::phys_to_virt(p.phys) + (lo - pstart) as usize) as *mut u8;
        let s = unsafe { src.add((lo - pos) as usize) };
        // A write-back of this very frame (`pc_writeback_page`): nothing to copy.
        if s as usize == d as usize { continue; }
        // The filesystem copied the same bytes from `src` a moment ago; a
        // fault now leaves the page as it was (the caller gets its count).
        let _ = unsafe { sched::uaccess::copy_raw(d, s, (hi - lo) as usize) };
    }
    mm::pagecache::wt_done(pos, done, &pins);
    if new_size != old_size { mm::pagecache::note_size(key, new_size); }
}

/// One request that may dirty the cache, under the lock, followed by the
/// write-back policy (`dispatch_locked`'s, for calls not shaped as messages).
fn run_mutating<R>(port: u32, mut f: impl FnMut(&mut MountState) -> R) -> Option<R> {
    let (r, throttle, kick) = with_mount(port, |ms| {
        let r = f(ms);
        let c = &ms.cache;
        let rw = !ms.read_only;
        (r, rw && c.npinned >= c.hard_pinned(), rw && c.ndirty >= c.soft_dirty())
    })?;
    if throttle {
        THROTTLE_COUNT.fetch_add(1, atomic::Ordering::Relaxed);
        let _ = checkpoint_unlocked(port, None, CpWhy::Throttle);
    } else if kick {
        kick_writeback(port);
    }
    Some(r)
}

/// Where a write goes.
#[derive(Clone, Copy)]
enum WritePos { At(u64), Append }

/// write(2)/pwrite(2) of `total` bytes from `buf` on open-file slot `slot`,
/// in `WRITE_CHUNK` pieces (each its own locked request and write-back
/// policy). Returns (bytes or -errno, offset just past the last byte
/// written — what a write(2) leaves the descriptor at).
fn write_at(port: u32, slot: usize, buf: u64, total: usize, at: WritePos, caller_pid: u32) -> (isize, u64) {
    let mut done = 0usize;
    let mut end_pos = match at { WritePos::At(p) => p, WritePos::Append => 0 };
    while done < total || total == 0 {
        let n = (total - done).min(WRITE_CHUNK);
        let want = match at { WritePos::At(p) => Some(p + done as u64), WritePos::Append => None };
        let src = (buf + done as u64) as *const u8;
        let r = run_mutating(port, |ms| -> Result<(u64, usize), i32> {
            act_code(VFS_WRITE);
            ms.cred = vfs_server::cred_of(caller_pid);
            if ms.read_only { return Err(-30); } // EROFS
            if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return Err(-9); }
            if !ms.open_files[slot].writable { return Err(-13); } // EACCES
            let ino = ms.open_files[slot].inode;
            // Per-file fsync (lane fsyncbatch): this change is `ino`'s.
            ms.req_ino = ino;
            let p = match want {
                Some(p) => p,
                // O_APPEND: the live inode's size, as handle_lseek's SEEK_END.
                None => {
                    let iblkaddr = nat_lookup(ms, ino);
                    let iblk = ms.cache.read(ms.dev, iblkaddr as u64);
                    inode_size(iblk)
                }
            };
            Ok((p, write_file_data(ms, ino, p, src, n)))
        });
        match r {
            None => return (if done > 0 { done as isize } else { -5 }, end_pos),
            Some(Err(e)) => return (if done > 0 { done as isize } else { e as isize }, end_pos),
            Some(Ok((p, w))) => {
                done += w;
                end_pos = p + w as u64;
                if w < n || total == 0 { break; }
            }
        }
    }
    (done as isize, end_pos)
}

/// Write one dirty page back (page-cache write-back: msync, fsync, the
/// background task). True if the filesystem took it.
fn pc_writeback_page(d: &mm::pagecache::DirtyPage) -> bool {
    let (port, ino) = key_port_ino(d.key);
    let size = match mm::pagecache::file_size(d.key) { Some(s) => s, None => return false };
    let start = d.pgoff * BLOCK_SIZE as u64;
    if start >= size { return true; } // past EOF: nothing of it belongs to the file
    let len = ((size - start) as usize).min(BLOCK_SIZE);
    let src = mm::phys_to_virt(d.phys) as *const u8;
    let r = run_mutating(port, |ms| {
        act_code(VFS_WRITE);
        if ms.read_only { return 0; }
        // The page must still be this file's dirty cached page: an inode
        // freed meanwhile dropped it (under this lock) and must not be
        // written.
        if !mm::pagecache::still_dirty(d.key, d.pgoff, d.phys) { return len; }
        ms.req_ino = ino; // per-file fsync must see this write
        write_file_data(ms, ino, start, src, len)
    });
    r == Some(len)
}

/// Write back dirty pages of the mount at `port` (all mounts: None).
/// `mapped`: also pages a shared mapping still holds (msync/fsync/sync) —
/// those stay dirty (the mapping may store again), so they are taken in one
/// pass; otherwise only pages no mapping can dirty again, until none is
/// left.
fn pc_flush_dirty(port: Option<u32>, mapped: bool) -> usize {
    if mm::pagecache::dirty_pages() == 0 { return 0; }
    let (lo, hi) = match port {
        Some(p) => (page_cache_key(p, 0), page_cache_key(p, u32::MAX)),
        None => (0, u64::MAX),
    };
    let mut written = 0usize;
    let mut batch: alloc::vec::Vec<mm::pagecache::DirtyPage> = alloc::vec::Vec::new();
    let passes = if mapped { 1 } else { 64 };
    for _ in 0..passes {
        batch.clear();
        mm::pagecache::take_dirty(lo, hi, mapped, if mapped { usize::MAX } else { 64 }, &mut batch);
        if batch.is_empty() { break; }
        let mut progress = false;
        for d in batch.iter() {
            let ok = pc_writeback_page(d);
            if ok { written += 1; progress = true; }
            mm::pagecache::writeback_done(d, ok);
        }
        if !progress { break; }
    }
    written
}

/// msync(2) of a shared mapping of the file `key`: write its dirty pages
/// back; with `sync`, also commit them (a checkpoint).
pub fn pcache_msync(key: u64, sync: bool) -> isize {
    let (port, _) = key_port_ino(key);
    let mut rc = if pc_flush_dirty_keys(key, key) { 0isize } else { -5 };
    if sync && checkpoint_unlocked(port, None, CpWhy::Fsync).is_err() { rc = -5; }
    rc
}

/// Every dirty page of the files with keys in `lo..=hi`, mapped or not, in
/// one pass, as ordinary write requests (fsync of one file, msync). False
/// if a page could not be written.
fn pc_flush_dirty_keys(lo: u64, hi: u64) -> bool {
    if mm::pagecache::dirty_pages() == 0 { return true; }
    let mut batch: alloc::vec::Vec<mm::pagecache::DirtyPage> = alloc::vec::Vec::new();
    mm::pagecache::take_dirty(lo, hi, true, usize::MAX, &mut batch);
    let mut all = true;
    for d in batch.iter() {
        let ok = pc_writeback_page(d);
        all &= ok;
        mm::pagecache::writeback_done(d, ok);
    }
    all
}

/// Background page-cache task: keeps free memory above the watermark,
/// writes back dirty pages no mapping holds any more, purges, and frees the
/// pages of orphaned files nobody has open.
fn pcache_thread() -> ! {
    let mut tick = 0u64;
    loop {
        nap(100_000_000);
        tick += 1;
        // The cache's spin locks must never be held across a preemption
        // (an IRQ-masked acquirer elsewhere could then spin forever): run
        // the work with interrupts masked, as syscalls do.
        let f = sched::ksync::irq_save();
        mm::pagecache::maybe_reclaim();
        if tick % 10 == 0 {
            mm::pagecache::purge();
            pc_flush_dirty(None, false);
        }
        if tick % 300 == 0 { pc_sweep_orphans(); }
        sched::ksync::irq_restore(f);
    }
}

fn start_pcache_thread() {
    static STARTED: atomic::AtomicBool = atomic::AtomicBool::new(false);
    if STARTED.swap(true, atomic::Ordering::AcqRel) { return; }
    if sched::spawn(pcache_thread, 0).is_none() {
        dbg_str("[pcache] could not start the page-cache task\n");
    }
}

/// Free the cached pages of files whose last name is gone and that nobody
/// has open or mapped any more.
fn pc_sweep_orphans() {
    let mut keys: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
    mm::pagecache::orphans(&mut keys);
    for key in keys {
        let (port, ino) = key_port_ino(key);
        let open = with_mount(port, |ms| ino_is_open(ms, ino)).unwrap_or(false);
        if !open { mm::pagecache::drop_file(key); }
    }
}

/// pread(2) on open-file slot `file_id`: positional, through the page
/// cache, the descriptor's offset untouched (and not locked: Linux's pread
/// does not take f_pos_lock either).
pub fn pread_fd(port: u32, file_id: u64, dst: *mut u8, len: usize, pos: u64) -> isize {
    let ino = match slot_fast(port, file_id as usize) {
        Some(s) => s.ino.load(atomic::Ordering::Acquire),
        None => 0,
    };
    if ino == 0 { return -9; } // EBADF
    let r = pc_read(port, ino, pos, dst, len, PcMode::Read);
    if r > 0 { pc_atime(port, ino); }
    r
}

/// pwrite(2) on open-file slot `file_id` at `pos` (at EOF if `append`, as
/// Linux does for an O_APPEND descriptor); the offset is untouched.
pub fn pwrite_fd(port: u32, file_id: u64, src: *const u8, len: usize, pos: u64, append: bool,
                 caller_pid: u32) -> isize {
    let at = if append { WritePos::Append } else { WritePos::At(pos) };
    write_at(port, file_id as usize, src as u64, len, at, caller_pid).0
}


/// Planning rounds `read_unlocked` tries (each re-plans after the file
/// changed under an unlocked read) before it reads the file through the
/// cache instead, still without I/O under the lock.
const UNLOCKED_READ_TRIES: u32 = 4;

/// A device run planned under the lock: `nblk` blocks at `phys` into
/// `dst + off`.
#[derive(Clone, Copy)]
struct Run { off: usize, phys: u32, nblk: usize }

// ── Exhaustion diagnostics ────────────────────────────────────────────────────
//
// `open` answering errno 24 says "too many open files" and userspace reads
// that as its OWN fd limit, which is the one thing it is not: the pool that
// ran out is this server's, per mount and shared by every process on the
// volume. That misdirection cost a full investigation once already, so the
// site names the table it actually exhausted — the same contract, and the same
// once-per-boot budget, as `report_fd_tables_full` in servers/vfs.

extern "C" { fn arch_serial_putc(c: u8); }

fn dbg_str(msg: &str) {
    for &b in msg.as_bytes() { unsafe { arch_serial_putc(b) }; }
}

fn dbg_dec(mut v: usize) {
    if v == 0 { unsafe { arch_serial_putc(b'0') }; return; }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 { i -= 1; buf[i] = b'0' + (v % 10) as u8; v /= 10; }
    for &b in &buf[i..] { unsafe { arch_serial_putc(b) }; }
}

/// A mount's open-file array is full, so `open` returns EMFILE. This is
/// `f2fs::MAX_OPEN_FILES` — a per-mount, system-wide pool that directories
/// occupy too — and not the calling process's fd limit. Printed once per boot:
/// the pool tends to sit at its ceiling once it reaches it, and the console
/// costs ~0.19 s per line.
fn report_open_files_full() {
    static REPORTED: atomic::AtomicBool = atomic::AtomicBool::new(false);
    if REPORTED.swap(true, atomic::Ordering::Relaxed) { return; }
    dbg_str("\n[F2FS] EMFILE: mount open-file pool FULL at ");
    dbg_dec(MAX_OPEN_FILES);
    dbg_str(" slots -- this is f2fs::MAX_OPEN_FILES, a per-mount pool shared by\n");
    dbg_str("[F2FS] EVERY process on the volume (open directories included), NOT the\n");
    dbg_str("[F2FS] caller's own fd limit. Callers that swallow the error (icon-theme\n");
    dbg_str("[F2FS] and font scans do) stay silently degraded for the rest of their life.\n");
}

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
    // Most calls re-record an unchanged entry (an inode rewritten in place
    // after every write): leave the NAT block clean then, or every fsync's
    // checkpoint pays one more scattered write for it (lane f2fsperf).
    {
        let cur = ms.cache.read(ms.dev, nat_blk as u64);
        if cur[off] == 1 && r32(cur, off + 1) == ino && r32(cur, off + 5) == blk_addr { return; }
    }
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
    // Mirror exactly what the vblocks field did (including the 10-bit wrap),
    // so `valid_blocks` always equals a full SIT sum.
    ms.valid_blocks = (ms.valid_blocks + (new_v & SIT_VBLOCKS_MASK) as u64)
        .wrapping_sub(old_cnt as u64);
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
    let old_cnt = v & SIT_VBLOCKS_MASK;
    let cnt = old_cnt.saturating_sub(1);
    w16(blk, entry_off, (v & !SIT_VBLOCKS_MASK) | cnt);
    ms.valid_blocks = (ms.valid_blocks + cnt as u64).wrapping_sub(old_cnt as u64);
    // See sit_mark_block_used on why free_seg_cnt is not touched. What matters
    // for reclaim is that the valid_map bit is cleared and vblocks decremented,
    // so sit_find_free_seg will hand this block's segment back once it empties.
}

/// Release a single physical block back to the allocator. No-op for holes.
///
/// Returns whether a real block was actually released, so the truncate walk can
/// refund exactly that many against the inode's `i_blocks` — a hole costs
/// nothing and must not be refunded, or a sparse file's count goes negative.
fn free_block(ms: &mut MountState, phys: u32) -> bool {
    if let Some((seg, blk)) = blkaddr_to_seg(ms, phys) {
        sit_mark_block_free(ms, seg, blk);
        return true;
    }
    false
}

/// Sum the valid-block counts across every main-area segment, reading each SIT
/// block *through the cache* so counts dirtied by an alloc or a reclaim that
/// has not been checkpointed yet are still seen. `statfs` reports free space as
/// `user_blocks - this`, so it is current the instant a block is used or freed.
///
/// O(SIT): about 62 SIT blocks on the 2 GiB root, each a device read on a
/// miss. It runs once per mount to seed `MountState::valid_blocks`, which
/// statfs reads (lane f2fsperf: on the PC's USB stick a statfs held the
/// filesystem lock for 5+ s doing this walk); `f2fs.statfs_check=1` on the
/// kernel command line re-runs it on every statfs as a cross-check.
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

/// Hand out one data block, **guaranteed to read back as zeros**.
///
/// The zeroing is not defensive tidiness, it is the hole contract. A write that
/// does not cover a whole block goes through `cache.get_mut`, i.e. read the
/// block off the device, overlay the new bytes, write it back. For a block that
/// was just carved out of free space, "off the device" is whatever the segment
/// held in its previous life — a deleted file's contents, or mkfs's fill. So a
/// partial write into a hole published that stale data as the rest of the
/// block, and a read of the never-written part of the file returned it. The
/// probe for disks-rs's GPT pattern caught exactly this: the backup GPT starts
/// 34 KiB before EOF, i.e. mid-block, and the 2 KiB of hole ahead of it came
/// back as the recycled segment's old bytes instead of zeros. It is also a
/// cross-file information leak, which is the reason it is fixed here (at the
/// source of every data block) rather than at the one write path that noticed.
///
/// Free, in practice: the zeros go into the block cache, so a full-block write
/// immediately afterwards replaces the same slot with no device I/O, and any
/// stale *dirty* cache entry left over from the block's previous owner is
/// dropped rather than being written back over the new contents.
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
    let zeros = [0u8; BLOCK_SIZE];
    ms.cache.write(ms.dev, phys as u64, &zeros);
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
    // A read-only mount has nothing dirty (the switch flushed it) and must
    // not replace its clean-unmount checkpoint with an ordinary one.
    if ms.read_only { return; }
    // Nothing changed since the last checkpoint (an fsync after an fsync,
    // the periodic tick): the volume is already committed, and on the PC's
    // stick the checkpoint block alone is a ~0.5 s write (lane fsio).
    if ms.dirty_writes == 0 && cp_pos(ms) == ms.last_cp_pos
        && ms.cache.ndirty == 0 { return; }
    write_checkpoint(ms, false);
    CP_COUNT.fetch_add(1, atomic::Ordering::Relaxed);
}

/// The allocation state a checkpoint records (compared to skip a checkpoint
/// that would commit nothing new).
fn cp_pos(ms: &MountState) -> [u32; 6] {
    [ms.cp.cur_node_segno, ms.cp.cur_node_blkoff as u32, ms.cp.cur_data_segno,
     ms.cp.cur_data_blkoff as u32, ms.cp.next_free_nid, ms.cp.free_seg_cnt]
}

/// Flush the block cache and commit a checkpoint; `umount` marks it as the
/// checkpoint of a clean unmount (`CP_UMOUNT_FLAG`), which the next mount
/// reports as a clean shutdown. Synchronous, with the lock held: unmount,
/// remount-ro, shutdown and contexts that may not sleep only.
fn write_checkpoint(ms: &mut MountState, umount: bool) {
    act_code(ACT_CHECKPOINT);
    let seq = cp_seq_begin(ms.port);
    ms.cache.flush_all(ms.dev);
    // Barrier: every data/metadata block flushed above must be on the medium
    // before the checkpoint block (the commit record) is written. Without this,
    // a crash inside this function could make the CP durable ahead of the blocks
    // it commits, yielding a checkpoint that points at not-yet-written data.
    io_flush(ms.dev);

    let (cp_blkno, buf) = build_cp_block(ms, umount);
    io_write_block(ms.dev, cp_blkno, &buf);
    // The checkpoint block is the commit record for everything flushed above
    // it, so it is the one write that must actually be on the medium before
    // this function claims the volume is consistent.
    io_flush(ms.dev);

    ms.dirty_writes = 0;
    ms.last_cp_ns = sched::monotonic_ns();
    ms.last_cp_pos = cp_pos(ms);
    cp_seq_done(ms.port, seq);
}

/// The next checkpoint's commit block (and the in-memory state that records
/// it as written: version, active pack, flags). Shared by the locked
/// `write_checkpoint` and the unlocked `checkpoint_unlocked`.
fn build_cp_block(ms: &mut MountState, umount: bool) -> (u64, [u8; BLOCK_SIZE]) {
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
    ms.cp.flags = if umount { CP_UMOUNT_FLAG } else { 0 };
    w32(&mut buf, CP_CKPT_FLAGS,      ms.cp.flags);
    (cp_blkno as u64, buf)
}

// ── Unlocked checkpoints (lane loginhang, 2026-10-10) ─────────────────────────
//
// A checkpoint used to write every dirty cached block back with F2FS_MOUNTS
// held. On a USB stick (~40 ms per small write on the PC, nvrm-boot10.log) a
// COSMIC session's fsyncs keep checkpointing several times a second; in the
// slow-stick emulation the lock was then held ~100% of the time and every
// login, exec or `cat` queued behind whole checkpoints (1.6-2 s per login).
//
// Now fsync, the background write-back and a throttled writer run a
// checkpoint in three phases:
//   1. lock held: copy every dirty block (into coalesced runs) and mark its
//      slot clean-but-pinned (`wb`), build the commit block (state as of
//      this instant), release;
//   2. no filesystem lock: write the runs, flush the device, write the
//      commit block, flush;
//   3. lock held briefly: unpin.
// Checkpoints are serialised by `CP_MUTEX` (taken before F2FS_MOUNTS, never
// inside it). Only the synchronous checkpoint (unmount, remount-ro,
// shutdown) writes cache blocks with the lock held, and it first waits for
// phase 2 to finish (`wait_wb_idle`), so an older copy can never land on top
// of a newer write. A pinned slot is never evicted (lane f2fsperf), so no
// normal path waits for phase 2 with the lock held. Crash consistency is that
// of the locked checkpoint: the commit block describes exactly the snapshot,
// and is written after all of it is on the medium.
//
// Group commit (lane f2fsperf): each mount counts checkpoints started (at
// the start of phase 1, under the lock) and completed (`CP_SEQ`). An fsync
// notes `started + 1` on arrival; once it holds CP_MUTEX, a checkpoint with
// at least that number that has completed began after the fsync's data was
// in the cache, so the fsync returns without writing anything. Concurrent
// fsyncs (sqlite's journal + db, several apps) therefore share one
// checkpoint instead of queueing one each.

static CP_MUTEX: sched::ksync::KMutex<()> = sched::ksync::KMutex::new(());
/// Phase 2 of an unlocked checkpoint is running.
static WB_INFLIGHT: atomic::AtomicBool = atomic::AtomicBool::new(false);
/// Diagnostics for the `[fslock]` summary: checkpoints, fsyncs, fsyncs
/// that shared another's checkpoint and throttled writers since the last
/// window, and the last fsync caller.
static CP_COUNT: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static FSYNC_COUNT: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static FSYNC_SHARED: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static THROTTLE_COUNT: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static FSYNC_LAST_PID: atomic::AtomicU32 = atomic::AtomicU32::new(0);

/// Per-mount checkpoint sequence numbers (group commit). `port` 0 = unused.
struct CpSeq {
    port: atomic::AtomicU32,
    started: atomic::AtomicU64,
    done: atomic::AtomicU64,
    /// Delayed group commit (lane fsyncbatch): arrival of the first fsync
    /// waiting for the next checkpoint (0 = none); reset when a checkpoint
    /// takes its snapshot.
    pending_t0: atomic::AtomicU64,
    /// `monotonic_ns` when the last checkpoint finished writing.
    last_end: atomic::AtomicU64,
    /// `f2fs.fsync=lazy`: the time by which a checkpoint must start for the
    /// fsyncs that returned early (0 = none owed); reset at a snapshot.
    lazy_due: atomic::AtomicU64,
}
static CP_SEQ: [CpSeq; MAX_MOUNTS] = [const {
    CpSeq { port: atomic::AtomicU32::new(0), started: atomic::AtomicU64::new(0), done: atomic::AtomicU64::new(0),
            pending_t0: atomic::AtomicU64::new(0), last_end: atomic::AtomicU64::new(0),
            lazy_due: atomic::AtomicU64::new(0) }
}; MAX_MOUNTS];

fn cp_seq(port: u32) -> Option<&'static CpSeq> {
    CP_SEQ.iter().find(|s| s.port.load(atomic::Ordering::Acquire) == port)
}

/// A checkpoint of `port` starts (call under F2FS_MOUNTS, before the
/// snapshot is taken): its sequence number. Everything in the cache now is
/// in it, so the delayed-commit window and any lazy-fsync promise close.
fn cp_seq_begin(port: u32) -> u64 {
    cp_seq(port).map_or(0, |s| {
        s.pending_t0.store(0, atomic::Ordering::SeqCst);
        s.lazy_due.store(0, atomic::Ordering::SeqCst);
        s.started.fetch_add(1, atomic::Ordering::SeqCst) + 1
    })
}

/// Checkpoint `seq` of `port` is on the medium.
fn cp_seq_done(port: u32, seq: u64) {
    if let Some(s) = cp_seq(port) { s.done.fetch_max(seq, atomic::Ordering::SeqCst); }
}

/// Block requests issued with F2FS_MOUNTS held since the previous call.
pub fn locked_io_window() -> u32 {
    LOCKED_IO_COUNT.swap(0, atomic::Ordering::Relaxed)
}

/// `(checkpoints, fsyncs, fsyncs served by a shared checkpoint, throttled
/// writers, last fsync pid)` since the previous call.
pub fn checkpoint_window() -> (u32, u32, u32, u32, u32) {
    (CP_COUNT.swap(0, atomic::Ordering::Relaxed), FSYNC_COUNT.swap(0, atomic::Ordering::Relaxed),
     FSYNC_SHARED.swap(0, atomic::Ordering::Relaxed), THROTTLE_COUNT.swap(0, atomic::Ordering::Relaxed),
     FSYNC_LAST_PID.load(atomic::Ordering::Relaxed))
}

/// Wait until no unlocked checkpoint is in its device-write phase. Called
/// with F2FS_MOUNTS held (synchronous checkpoints only): phase 2 needs only
/// the disk, never this lock.
fn wait_wb_idle() {
    while WB_INFLIGHT.load(atomic::Ordering::Acquire) {
        if sched::irqctx::can_sleep() && sched::current_pid() != 0 {
            let d = sched::monotonic_ns().saturating_add(500_000);
            sched::block_on_poll_prepare_masked(d, sched::POLL_MASK_TIMED_ONLY);
            if sched::monotonic_ns() >= d || !WB_INFLIGHT.load(atomic::Ordering::Acquire) {
                sched::block_on_poll_cancel();
            } else {
                sched::block_on_poll_commit();
            }
        } else {
            core::hint::spin_loop();
        }
    }
}

/// Why a checkpoint runs (the `[ckpt]` summary).
#[derive(Clone, Copy, PartialEq)]
enum CpWhy { Fsync = 0, Sync = 1, Age = 2, Pressure = 3, Throttle = 4, Lazy = 5 }
const CP_WHYS: usize = 6;
const CP_WHY_NAMES: [&str; CP_WHYS] = ["fsync", "sync", "age", "pressure", "throttle", "lazy"];

/// What a `checkpoint_unlocked` call did, for its caller's statistics.
#[derive(Default, Clone, Copy)]
struct CpOut {
    /// Time spent waiting for CP_MUTEX.
    mutex_wait_ns: u64,
    /// This call wrote a checkpoint (false: shared one, or nothing to do).
    ran: bool,
}

/// A checkpoint (unlocked or not) is running: CP_MUTEX is held.
static CP_RUNNING: atomic::AtomicBool = atomic::AtomicBool::new(false);
/// Moving average of a checkpoint's device-write phase (ns): bounds the
/// delayed-commit window on fast devices (lane fsyncbatch).
static CP_EWMA_NS: atomic::AtomicU64 = atomic::AtomicU64::new(0);

/// Checkpoint the mount behind `port` with the device writes done outside
/// F2FS_MOUNTS (see above). With `need` (an fsync's ticket), return at once
/// if a checkpoint numbered `need` or later has already completed.
/// Task context that may sleep; otherwise it falls back to the locked
/// checkpoint. Err(()) if a device write failed (nothing was committed).
fn checkpoint_unlocked(port: u32, need: Option<u64>, why: CpWhy) -> Result<(), ()> {
    checkpoint_unlocked_ex(port, need, why, &mut CpOut::default())
}

fn checkpoint_unlocked_ex(port: u32, need: Option<u64>, why: CpWhy, out: &mut CpOut) -> Result<(), ()> {
    if !sched::irqctx::can_sleep() || sched::current_pid() == 0 {
        note_atomic_request();
        let mut mounts = lock_mounts();
        if let Some(ms) = mounts.iter_mut().flatten().find(|m| m.port == port) { flush_checkpoint(ms); }
        return Ok(());
    }
    let tw = sched::monotonic_ns();
    let _cp = CP_MUTEX.lock();
    out.mutex_wait_ns = sched::monotonic_ns().saturating_sub(tw);
    CP_RUNNING.store(true, atomic::Ordering::SeqCst);
    let r = checkpoint_mutex_held(port, need, why, out);
    CP_RUNNING.store(false, atomic::Ordering::SeqCst);
    r
}

/// Write regions of a checkpoint (`[ckpt]`): main-area data, main-area node
/// blocks, NAT, SIT, the rest of the metadata area (SSA), the commit block.
const REG_DATA: usize = 0;
const REG_NODE: usize = 1;
const REG_NAT: usize = 2;
const REG_SIT: usize = 3;
const REG_META: usize = 4;
const REG_CP: usize = 5;
const REGS: usize = 6;
const REG_NAMES: [&str; REGS] = ["data", "node", "nat", "sit", "ssa", "cp"];

/// Region of a block being written back. A main-area block is a node block
/// when its footer names a nid whose NAT entry (if that NAT block is cached;
/// it nearly always is, the node was just reached through it) points at it.
fn block_region(ms: &MountState, blk: u64, data: &[u8; BLOCK_SIZE]) -> usize {
    let sb = &ms.sb;
    let nat_end = sb.nat_blkaddr as u64 + sb.seg_cnt_nat as u64 * sb.blocks_per_seg as u64;
    if blk >= sb.main_blkaddr as u64 {
        let nid = r32(data, NODE_FOOTER_OFF);
        if nid != 0 {
            let nat_blk = sb.nat_blkaddr as u64 + (nid / NAT_ENTRY_PER_BLK as u32) as u64;
            if let Some(i) = ms.cache.find(nat_blk) {
                let off = (nid % NAT_ENTRY_PER_BLK as u32) as usize * NAT_ENTRY_SIZE + 5;
                if r32(ms.cache.data(i), off) as u64 == blk { return REG_NODE; }
            }
        }
        REG_DATA
    } else if blk >= sb.nat_blkaddr as u64 && blk < nat_end {
        REG_NAT
    } else if blk >= sb.sit_blkaddr as u64 && blk < sb.nat_blkaddr as u64 {
        REG_SIT
    } else if blk >= sb.cp_blkaddr as u64 && blk < sb.sit_blkaddr as u64 {
        REG_CP
    } else {
        REG_META
    }
}

/// Checkpoint statistics since the last `[ckpt]` line.
struct CpWin {
    n: [u32; CP_WHYS],
    blocks: [u32; REGS],
    cmds: [u32; REGS],
    ns: [u64; REGS],
    flush_ns: u64,
    total_ns: u64,
    max_ns: u64,
}
static CP_WIN: Mutex<CpWin> = Mutex::new(CpWin {
    n: [0; CP_WHYS], blocks: [0; REGS], cmds: [0; REGS], ns: [0; REGS], flush_ns: 0, total_ns: 0, max_ns: 0,
});

fn checkpoint_mutex_held(port: u32, need: Option<u64>, why: CpWhy, out: &mut CpOut) -> Result<(), ()> {
    if let (Some(n), Some(s)) = (need, cp_seq(port)) {
        if s.done.load(atomic::Ordering::SeqCst) >= n {
            FSYNC_SHARED.fetch_add(1, atomic::Ordering::Relaxed);
            return Ok(());
        }
    }
    // Phase 1.
    let (dev, runs, run_reg, reg_blocks, cp_blkno, cp_buf, seq) = {
        let mut mounts = lock_mounts();
        let ms = match mounts.iter_mut().flatten().find(|m| m.port == port) { Some(m) => m, None => return Ok(()) };
        if ms.read_only { return Ok(()); }
        let seq = cp_seq_begin(port);
        if ms.dirty_writes == 0 && cp_pos(ms) == ms.last_cp_pos && ms.cache.ndirty == 0 {
            // Nothing new since the last checkpoint: committed already.
            cp_seq_done(port, seq);
            return Ok(());
        }
        act_code(ACT_CHECKPOINT);
        let (idx, bounds, mut runs) = match snapshot_plan(&ms.cache) {
            Some(p) => p,
            None => {
                dbg_str("[F2FS] checkpoint: out of memory for the snapshot; writing back under the lock\n");
                flush_checkpoint(ms);
                cp_seq_done(port, seq);
                return Ok(());
            }
        };
        let mut run_reg: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let _ = run_reg.try_reserve_exact(bounds.len());
        let mut reg_blocks = [0u32; REGS];
        for (r, &(k, n)) in bounds.iter().enumerate() {
            for j in 0..n {
                let i = idx[k + j];
                let reg = block_region(ms, ms.cache.meta[i].blk_no, ms.cache.data(i));
                reg_blocks[reg] += 1;
                if j == 0 && run_reg.len() < run_reg.capacity() { run_reg.push(reg as u8); }
                runs[r].1.extend_from_slice(ms.cache.data(i));
                ms.cache.set_flags(i, false, true);
            }
        }
        let (cp_blkno, cp_buf) = build_cp_block(ms, false);
        ms.dirty_writes = 0;
        ms.last_cp_ns = sched::monotonic_ns();
        ms.last_cp_pos = cp_pos(ms);
        WB_INFLIGHT.store(true, atomic::Ordering::Release);
        drop(idx);
        (ms.dev, runs, run_reg, reg_blocks, cp_blkno, cp_buf, seq)
    };
    out.ran = true;
    // Phase 2: the device writes, no filesystem lock. Each run goes out in
    // requests of at most `WB_IO_BLOCKS`, one USB MSC command each, so the
    // disk lock is released between commands: a read queued behind this
    // write-back (often by a task holding F2FS_MOUNTS for a metadata miss)
    // waits for one write command, not for a whole run (measured: a 960 KiB
    // run held the disk 2 s on the emulated 250 ms-per-write stick). The
    // runs are already in block order with adjacent blocks merged
    // (`snapshot_plan`), which is as sequential as the layout allows.
    let t_p2 = sched::monotonic_ns();
    let mut reg_ns = [0u64; REGS];
    let mut reg_cmds = [0u32; REGS];
    let mut ok = true;
    for (r, (start, data)) in runs.iter().enumerate() {
        let reg = run_reg.get(r).map_or(REG_DATA, |&g| g as usize);
        let t_run = sched::monotonic_ns();
        for (p, piece) in data.chunks(WB_IO_BLOCKS * BLOCK_SIZE).enumerate() {
            let at = *start + (p * WB_IO_BLOCKS) as u64;
            reg_cmds[reg] += 1;
            if piece.len() == BLOCK_SIZE {
                ok &= virtio_blk::write_block(dev, at, piece.try_into().unwrap());
            } else if !virtio_blk::write_blocks(dev, at, piece) {
                for (j, b) in piece.chunks_exact(BLOCK_SIZE).enumerate() {
                    ok &= virtio_blk::write_block(dev, at + j as u64, b.try_into().unwrap());
                }
            }
        }
        reg_ns[reg] += sched::monotonic_ns().saturating_sub(t_run);
    }
    let t_f = sched::monotonic_ns();
    virtio_blk::flush(dev);
    let mut flush_ns = sched::monotonic_ns().saturating_sub(t_f);
    if ok {
        let t_c = sched::monotonic_ns();
        ok &= virtio_blk::write_block(dev, cp_blkno, &cp_buf);
        let t_f2 = sched::monotonic_ns();
        reg_ns[REG_CP] += t_f2.saturating_sub(t_c);
        reg_cmds[REG_CP] += 1;
        virtio_blk::flush(dev);
        flush_ns += sched::monotonic_ns().saturating_sub(t_f2);
    }
    WB_INFLIGHT.store(false, atomic::Ordering::Release);
    let t_end = sched::monotonic_ns();
    let dur = t_end.saturating_sub(t_p2);
    CP_COUNT.fetch_add(1, atomic::Ordering::Relaxed);
    {
        let e = CP_EWMA_NS.load(atomic::Ordering::Relaxed);
        CP_EWMA_NS.store(if e == 0 { dur } else { (e * 3 + dur) / 4 }, atomic::Ordering::Relaxed);
        let mut w = CP_WIN.lock();
        w.n[why as usize] += 1;
        for k in 0..REGS {
            w.blocks[k] += reg_blocks[k];
            w.cmds[k] += reg_cmds[k];
            w.ns[k] += reg_ns[k];
        }
        w.blocks[REG_CP] += 1;
        w.flush_ns += flush_ns;
        w.total_ns += dur;
        w.max_ns = w.max_ns.max(dur);
    }
    if let Some(s) = cp_seq(port) { s.last_end.store(t_end, atomic::Ordering::SeqCst); }
    if ok { cp_seq_done(port, seq); }
    // Phase 3: unpin; on a write error the blocks are dirty again (the next
    // checkpoint retries them; the commit block was not written).
    let mut mounts = lock_mounts();
    if let Some(ms) = mounts.iter_mut().flatten().find(|m| m.port == port) {
        for (start, data) in &runs {
            for j in 0..(data.len() / BLOCK_SIZE) as u64 {
                if let Some(i) = ms.cache.find(start + j) {
                    if ms.cache.meta[i].wb {
                        let d = ms.cache.meta[i].dirty || !ok;
                        ms.cache.set_flags(i, d, false);
                    }
                }
            }
        }
        if !ok { ms.dirty_writes = ms.dirty_writes.max(1); }
    }
    // Lazy fsyncs that returned before this commit are owed another one.
    if !ok { lazy_owe(port, 1_000_000_000); }
    if ok { Ok(()) } else { Err(()) }
}

/// Phase 1's plan: the dirty slots in block order, their runs of adjacent
/// blocks (`(first index, length)`), and an empty buffer for each run. Every
/// allocation is made before any slot changes; None if memory ran out.
#[allow(clippy::type_complexity)]
fn snapshot_plan(c: &BlockCache) -> Option<(alloc::vec::Vec<usize>, alloc::vec::Vec<(usize, usize)>,
                                            alloc::vec::Vec<(u64, alloc::vec::Vec<u8>)>)> {
    let mut idx: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
    idx.try_reserve_exact(c.ndirty).ok()?;
    idx.extend((0..c.meta.len()).filter(|&i| c.meta[i].dirty));
    idx.sort_unstable_by_key(|&i| c.meta[i].blk_no);
    let mut bounds: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
    let mut k = 0;
    while k < idx.len() {
        let mut n = 1;
        while k + n < idx.len() && n < MAX_RUN
            && c.meta[idx[k + n]].blk_no == c.meta[idx[k]].blk_no + n as u64 { n += 1; }
        bounds.try_reserve(1).ok()?;
        bounds.push((k, n));
        k += n;
    }
    let mut runs: alloc::vec::Vec<(u64, alloc::vec::Vec<u8>)> = alloc::vec::Vec::new();
    runs.try_reserve_exact(bounds.len()).ok()?;
    for &(k, n) in &bounds {
        let mut v: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        v.try_reserve_exact(n * BLOCK_SIZE).ok()?;
        runs.push((c.meta[idx[k]].blk_no, v));
    }
    Some((idx, bounds, runs))
}

/// fsync(fd) — the file's data and metadata must be on stable storage when
/// this returns.
///
/// This server has no per-file dirty tracking: the block cache is shared
/// across the volume and the checkpoint is what makes any of it recoverable.
/// So an fsync of one file is necessarily a checkpoint of the whole volume —
/// heavier than Linux's, but honest, which the previous unconditional `0`
/// was not. (The IPC path, `f2fs_dispatch`, runs it unlocked and grouped.)
fn handle_fsync(ms: &mut MountState) -> Message {
    flush_checkpoint(ms);
    ok_reply()
}

// ── fsync policy (lane fsyncbatch, 2026-10-11) ────────────────────────────────
//
// boot16 (the PC's USB root, nvrm-boot16.log): 879 fsyncs and 527 whole-volume
// checkpoints in 701 s, about 0.76 checkpoints/s from an idle desktop, the
// stick spending 85% of its time on writes. Each checkpoint touches the data
// log, the node log, NAT, SIT and the commit block, and every write outside a
// recently written region costs that stick 0.25-1.1 s. Three changes:
//
// 1. Per-file fsync (fsync/fdatasync of a descriptor). Every change is
//    recorded with the number of the first checkpoint that will contain it
//    (`MountState::ino_mod` per inode, `mod_global` for changes not tied to
//    one inode: namespace operations, path-based requests, anything
//    `tracked` catches). An fsync waits only for the checkpoint its file's
//    latest change (or the latest unattributed change) needs; when that has
//    completed, the file is on the medium already and fsync returns at once.
//    (A whole volume with nothing dirty was already skipped.)
//
// 2. Delayed group commit (default on). An fsync that needs a new checkpoint
//    does not start it at once: the first such fsync opens a window, and the
//    checkpoint starts `delay` after the later of that arrival and the end of
//    the previous checkpoint, so every fsync arriving meanwhile shares it.
//    `delay` = min(`f2fs.fsync_delay_ms` (default 1000, 0 = off), 2 x the
//    recent checkpoint write time): about 1 s on the USB stick, a few ms on
//    virtio, so a fast disk is not slowed. Durability is unchanged: fsync
//    returns only after a checkpoint that started after the fsync arrived
//    (and after the file's last change) has completed.
//
// 3. `f2fs.fsync=lazy` (opt-in, default OFF). fsync and fdatasync return at
//    once; a checkpoint is guaranteed to START within `f2fs.lazy_s` seconds
//    (default 5) of the call. DATA-LOSS WINDOW: a power cut or hard reset in
//    that window loses everything written since the previous checkpoint,
//    fsynced or not (the volume itself stays consistent: it falls back to
//    the previous checkpoint, whole, exactly as an unclean shutdown always
//    has). sync(2), syncfs(2), unmount, remount-ro and poweroff/reboot stay
//    fully synchronous in this mode.

/// What the VFS asks for in `VFS_FSYNC` (data[1]): sync(2) (and old
/// callers that send nothing) commits everything; so does syncfs(2).
#[allow(dead_code)]
const FSYNC_KIND_SYNC: u64 = 0;
const FSYNC_KIND_FILE: u64 = 1;
const FSYNC_KIND_DATA: u64 = 2;
#[allow(dead_code)]
const FSYNC_KIND_SYNCFS: u64 = 3;

static FSYNC_CFG_LOADED: atomic::AtomicBool = atomic::AtomicBool::new(false);
/// `f2fs.fsyncbatch=0`: this lane off, for a one-boot A/B — every fsync
/// waits for a checkpoint started after it (no per-file skip, no delayed
/// group commit, no lazy mode) and a doomed run fetches only its first miss.
static FSYNCBATCH_ON: atomic::AtomicBool = atomic::AtomicBool::new(true);
static FSYNC_DELAY_CAP_NS: atomic::AtomicU64 = atomic::AtomicU64::new(1_000_000_000);
/// `f2fs.fsync=lazy`: the promised commit delay (0 = strict fsync).
static FSYNC_LAZY_NS: atomic::AtomicU64 = atomic::AtomicU64::new(0);

/// Read the fsync switches from the kernel command line (once, at the
/// first mount) and say what they are.
fn fsync_cfg_load() {
    if FSYNC_CFG_LOADED.swap(true, atomic::Ordering::AcqRel) { return; }
    if drivers::nvidia_probe::cmdline_opt("f2fs.fsyncbatch") == Some(0) {
        FSYNCBATCH_ON.store(false, atomic::Ordering::Relaxed);
        FSYNC_DELAY_CAP_NS.store(0, atomic::Ordering::Relaxed);
        dbg_str("[F2FS] f2fs.fsyncbatch=0: every fsync runs or joins a fresh checkpoint, one miss per fetch\n");
        return;
    }
    let delay_ms = drivers::nvidia_probe::cmdline_opt("f2fs.fsync_delay_ms").unwrap_or(1000).min(10_000);
    FSYNC_DELAY_CAP_NS.store(delay_ms * 1_000_000, atomic::Ordering::Relaxed);
    let lazy = drivers::nvidia_probe::cmdline_word("f2fs.fsync") == Some("lazy");
    let lazy_s = drivers::nvidia_probe::cmdline_opt("f2fs.lazy_s").unwrap_or(5).clamp(1, 600);
    if lazy { FSYNC_LAZY_NS.store(lazy_s * 1_000_000_000, atomic::Ordering::Relaxed); }
    let mut s = alloc::string::String::new();
    if lazy {
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!(
            "[F2FS] f2fs.fsync=lazy: fsync/fdatasync return before the data is on the medium; a checkpoint starts within {} s. A power cut or hard reset in that window LOSES the writes made since the previous checkpoint (the volume stays consistent). sync, syncfs, unmount and poweroff stay synchronous.\n",
            lazy_s));
    } else {
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!(
            "[F2FS] fsync: per-file, group commit delayed up to {} ms (f2fs.fsync_delay_ms; f2fs.fsync=lazy is off)\n",
            delay_ms));
    }
    dbg_str(&s);
}

/// The delayed-commit window now: the configured cap, but at most twice the
/// recent checkpoint write time (a few ms on virtio, the cap on the stick).
fn fsync_delay_ns() -> u64 {
    let cap = FSYNC_DELAY_CAP_NS.load(atomic::Ordering::Relaxed);
    cap.min(CP_EWMA_NS.load(atomic::Ordering::Relaxed).saturating_mul(2))
}

/// `f2fs.fsync=lazy`: owe the mount a checkpoint starting within `after_ns`.
fn lazy_owe(port: u32, after_ns: u64) {
    if FSYNC_LAZY_NS.load(atomic::Ordering::Relaxed) == 0 { return; }
    let Some(s) = cp_seq(port) else { return };
    let due = sched::monotonic_ns().saturating_add(after_ns).max(1);
    let mut cur = s.lazy_due.load(atomic::Ordering::SeqCst);
    loop {
        if cur != 0 && cur <= due { return; }
        match s.lazy_due.compare_exchange(cur, due, atomic::Ordering::SeqCst, atomic::Ordering::SeqCst) {
            Ok(_) => return,
            Err(v) => cur = v,
        }
    }
}

/// Delayed group commit: wait until the window of the next checkpoint closes
/// (`delay` after the later of the window's first fsync and the end of the
/// previous checkpoint, with no checkpoint running), or until a checkpoint
/// that will cover `need` has started or completed. Returns the time waited.
fn fsync_window(port: u32, s: &CpSeq, need: u64, delay: u64) -> u64 {
    let t_arr = sched::monotonic_ns();
    let t0 = match s.pending_t0.compare_exchange(0, t_arr, atomic::Ordering::SeqCst, atomic::Ordering::SeqCst) {
        Ok(_) => t_arr,
        Err(v) => v,
    };
    let give_up = t_arr.saturating_add(delay).saturating_add(30_000_000_000);
    loop {
        if s.done.load(atomic::Ordering::SeqCst) >= need || s.port.load(atomic::Ordering::Acquire) != port {
            break;
        }
        // Somebody started the checkpoint that covers this fsync: wait for
        // it on CP_MUTEX (`checkpoint_unlocked`), which shares it.
        if s.started.load(atomic::Ordering::SeqCst) >= need { break; }
        let now = sched::monotonic_ns();
        if now >= give_up { break; }
        let deadline = t0.max(s.last_end.load(atomic::Ordering::SeqCst)).saturating_add(delay);
        if now >= deadline && !CP_RUNNING.load(atomic::Ordering::SeqCst) { break; }
        let step = if now >= deadline { 5_000_000 } else { (deadline - now).min(20_000_000) };
        nap(step.max(1_000_000));
    }
    sched::monotonic_ns().saturating_sub(t_arr)
}

// ── fsync attribution (`[fsync]` / `[ckpt]` lines, every 5 s) ─────────────────

const FS_RECS: usize = 32;
const FS_PATH: usize = 48;

#[derive(Clone, Copy)]
struct FsRec {
    tgid: u32,
    ino: u32,
    n: u32,
    clean: u32,
    lazy: u32,
    ran: u32,
    lat_sum: u64,
    lat_max: u64,
    wait_sum: u64,
    wait_max: u64,
    delay_sum: u64,
    comm: [u8; 15],
    clen: u8,
    path: [u8; FS_PATH],
    plen: u8,
}

impl FsRec {
    const fn empty() -> Self {
        FsRec { tgid: 0, ino: 0, n: 0, clean: 0, lazy: 0, ran: 0, lat_sum: 0, lat_max: 0, wait_sum: 0,
                wait_max: 0, delay_sum: 0, comm: [0; 15], clen: 0, path: [0; FS_PATH], plen: 0 }
    }
}

struct FsWin {
    recs: [FsRec; FS_RECS],
    n: usize,
    /// Callers that did not fit in `recs` this window.
    overflow: u32,
    total: u32,
    clean: u32,
    lazy: u32,
    ran: u32,
    /// sync(2)/syncfs calls among `total`.
    full: u32,
    lat_max: u64,
}

static FS_WIN: Mutex<FsWin> = Mutex::new(FsWin {
    recs: [const { FsRec::empty() }; FS_RECS], n: 0, overflow: 0, total: 0, clean: 0, lazy: 0, ran: 0,
    full: 0, lat_max: 0,
});

/// One finished fsync for the attribution table.
struct FsSample<'a> {
    pid: u32,
    ino: u32,
    path: &'a [u8],
    full: bool,
    clean: bool,
    lazy: bool,
    ran: bool,
    lat: u64,
    wait: u64,
    delay: u64,
}

fn fsync_record(x: &FsSample) {
    let tgid = sched::tgid_of(x.pid);
    let mut comm = [0u8; 15];
    let mut clen = 0;
    let mut exe = [0u8; 256];
    if let Some(n) = sched::exe_path_try(tgid, &mut exe) {
        let e = &exe[..n.min(exe.len())];
        let base = e.rsplit(|&b| b == b'/').next().unwrap_or(e);
        clen = base.len().min(15);
        comm[..clen].copy_from_slice(&base[..clen]);
    }
    let mut w = FS_WIN.lock();
    w.total += 1;
    if x.full { w.full += 1; }
    if x.clean { w.clean += 1; }
    if x.lazy { w.lazy += 1; }
    if x.ran { w.ran += 1; }
    w.lat_max = w.lat_max.max(x.lat);
    let n = w.n;
    let k = match w.recs[..n].iter().position(|r| r.tgid == tgid && r.ino == x.ino) {
        Some(k) => k,
        None if n < FS_RECS => {
            let mut r = FsRec::empty();
            r.tgid = tgid;
            r.ino = x.ino;
            r.comm = comm;
            r.clen = clen as u8;
            let pl = x.path.len().min(FS_PATH);
            r.path[..pl].copy_from_slice(&x.path[x.path.len() - pl..]);
            r.plen = pl as u8;
            w.recs[n] = r;
            w.n = n + 1;
            n
        }
        None => { w.overflow += 1; return; }
    };
    let r = &mut w.recs[k];
    r.n += 1;
    if x.clean { r.clean += 1; }
    if x.lazy { r.lazy += 1; }
    if x.ran { r.ran += 1; }
    r.lat_sum += x.lat;
    r.lat_max = r.lat_max.max(x.lat);
    r.wait_sum += x.wait;
    r.wait_max = r.wait_max.max(x.wait);
    r.delay_sum += x.delay;
}

/// The `[fsync]` and `[ckpt]` summary lines for the window just ended (the
/// kernel's I/O summary task, every 5 s), or None if nothing happened. Resets
/// the window.
pub fn fsync_report(window_ns: u64) -> Option<alloc::string::String> {
    use core::fmt::Write;
    let ms = |ns: u64| ns / 1_000_000;
    let (recs, n, overflow, total, clean, lazy, ran, full, lat_max) = {
        let mut w = FS_WIN.lock();
        let r = (w.recs, w.n, w.overflow, w.total, w.clean, w.lazy, w.ran, w.full, w.lat_max);
        w.n = 0;
        w.overflow = 0;
        w.total = 0;
        w.clean = 0;
        w.lazy = 0;
        w.ran = 0;
        w.full = 0;
        w.lat_max = 0;
        r
    };
    let cw = {
        let mut w = CP_WIN.lock();
        let r = CpWin { n: w.n, blocks: w.blocks, cmds: w.cmds, ns: w.ns, flush_ns: w.flush_ns,
                        total_ns: w.total_ns, max_ns: w.max_ns };
        *w = CpWin { n: [0; CP_WHYS], blocks: [0; REGS], cmds: [0; REGS], ns: [0; REGS], flush_ns: 0,
                     total_ns: 0, max_ns: 0 };
        r
    };
    let ncp: u32 = cw.n.iter().sum();
    if total == 0 && ncp == 0 { return None; }
    let mut s = alloc::string::String::new();
    if total != 0 {
        let shared = total.saturating_sub(clean + lazy + ran);
        let _ = write!(s, "[fsync] {} ms: {} calls ({} sync, {} clean, {} lazy, {} shared, {} ran a checkpoint), max {} ms; top:",
                       ms(window_ns), total, full, clean, lazy, shared, ran, ms(lat_max));
        let mut order = [0usize; FS_RECS];
        for (i, o) in order.iter_mut().enumerate() { *o = i; }
        order[..n].sort_unstable_by(|&a, &b| recs[b].n.cmp(&recs[a].n));
        for &i in order[..n].iter().take(5) {
            let r = &recs[i];
            let comm = core::str::from_utf8(&r.comm[..r.clen as usize]).unwrap_or("?");
            let path = core::str::from_utf8(&r.path[..r.plen as usize]).unwrap_or("?");
            let _ = write!(s, " | {} {} ", r.tgid, if comm.is_empty() { "?" } else { comm });
            if r.ino == 0 {
                let _ = write!(s, "(sync)");
            } else if path.is_empty() {
                let _ = write!(s, "ino {}", r.ino);
            } else {
                let _ = write!(s, "{}{} ino {}", if r.plen as usize == FS_PATH { ".." } else { "" }, path, r.ino);
            }
            let _ = write!(s, " x{} ({} clean, {} lazy, {} ran) lat {}/{} ms delay {} ms cpwait {}/{} ms",
                           r.n, r.clean, r.lazy, r.ran, ms(r.lat_sum / r.n.max(1) as u64), ms(r.lat_max),
                           ms(r.delay_sum / r.n.max(1) as u64), ms(r.wait_sum / r.n.max(1) as u64), ms(r.wait_max));
        }
        if n > 5 { let _ = write!(s, " | {} more callers", n - 5); }
        if overflow != 0 { let _ = write!(s, " | {} calls untracked", overflow); }
        s.push('\n');
    }
    if ncp != 0 {
        let _ = write!(s, "[ckpt] {} ms: {} checkpoints (", ms(window_ns), ncp);
        let mut first = true;
        for k in 0..CP_WHYS {
            if cw.n[k] == 0 { continue; }
            let _ = write!(s, "{}{} {}", if first { "" } else { ", " }, CP_WHY_NAMES[k], cw.n[k]);
            first = false;
        }
        let _ = write!(s, "), writing {} ms (max {} ms):", ms(cw.total_ns), ms(cw.max_ns));
        for k in 0..REGS {
            let _ = write!(s, " {} {} blk/{} cmd {} ms;", REG_NAMES[k], cw.blocks[k], cw.cmds[k], ms(cw.ns[k]));
        }
        let _ = write!(s, " flush {} ms\n", ms(cw.flush_ns));
    }
    Some(s)
}

/// fsync(2)/fdatasync(2) of descriptor `file_id` (`kind`), or sync(2) /
/// syncfs(2) of the whole mount behind `port` (see above).
fn fsync_port(port: u32, caller_pid: u32, file_id: u64, kind: u64) -> Message {
    let t_arr = sched::monotonic_ns();
    FSYNC_COUNT.fetch_add(1, atomic::Ordering::Relaxed);
    FSYNC_LAST_PID.store(caller_pid, atomic::Ordering::Relaxed);
    let per_file = kind == FSYNC_KIND_FILE || kind == FSYNC_KIND_DATA;
    let mut ino = 0u32;
    let mut path = [0u8; FS_PATH];
    let mut plen = 0usize;
    // The checkpoint this call must see completed. No device I/O here.
    let need = {
        let mounts = lock_mounts();
        let next = next_cp_seq(port);
        let mut need = next;
        if let Some(ms) = mounts.iter().flatten().find(|m| m.port == port) {
            let fid = file_id as usize;
            if per_file && fid < MAX_OPEN_FILES && ms.open_files[fid].in_use {
                let f = &ms.open_files[fid];
                ino = f.inode;
                let pl = f.path_len.min(MAX_OPEN_PATH);
                let st = pl.saturating_sub(FS_PATH);
                plen = pl - st;
                path[..plen].copy_from_slice(&f.path[st..pl]);
                if FSYNCBATCH_ON.load(atomic::Ordering::Relaxed) {
                    need = ms.ino_mod[ino_mod_idx(ino)].max(ms.mod_global).min(next);
                }
            }
        }
        need
    };
    let s = cp_seq(port);
    let mut x = FsSample { pid: caller_pid, ino, path: &path[..plen], full: !per_file, clean: false,
                           lazy: false, ran: false, lat: 0, wait: 0, delay: 0 };
    if s.map_or(true, |s| s.done.load(atomic::Ordering::SeqCst) >= need) {
        // Nothing this file (or anything unattributed) changed since the
        // last completed checkpoint: on the medium already.
        x.clean = true;
        x.lat = sched::monotonic_ns().saturating_sub(t_arr);
        fsync_record(&x);
        return ok_reply();
    }
    let lazy_ns = FSYNC_LAZY_NS.load(atomic::Ordering::Relaxed);
    if per_file && lazy_ns != 0 {
        // `f2fs.fsync=lazy`: commit within the promised time, return now.
        // (The write-back task checks the promise every second.)
        lazy_owe(port, lazy_ns.saturating_sub(1_000_000_000));
        x.lazy = true;
        x.lat = sched::monotonic_ns().saturating_sub(t_arr);
        fsync_record(&x);
        return ok_reply();
    }
    if let (true, Some(s)) = (per_file, s) {
        let d = fsync_delay_ns();
        let can_wait = sched::irqctx::can_sleep() && sched::current_pid() != 0;
        if d != 0 && can_wait && need > s.started.load(atomic::Ordering::SeqCst) {
            x.delay = fsync_window(port, s, need, d);
        }
    }
    let mut out = CpOut::default();
    let r = checkpoint_unlocked_ex(port, Some(need), if per_file { CpWhy::Fsync } else { CpWhy::Sync }, &mut out);
    x.ran = out.ran;
    x.wait = out.mutex_wait_ns;
    x.lat = sched::monotonic_ns().saturating_sub(t_arr);
    fsync_record(&x);
    match r {
        Ok(()) => ok_reply(),
        Err(()) => err_reply(-5), // EIO: the commit block was not written
    }
}

/// Routine checkpoint interval (lane f2fsperf; was 5 s in lane fsio). The
/// background write-back commits changes once they are this old; dirty
/// pressure (`BlockCache::soft_dirty`), fsync, sync, remount-ro, unmount and
/// shutdown commit at once regardless. On the PC's stick (boot12) a 5 s
/// cadence plus one checkpoint per fsync was 142 checkpoints in 404 s of
/// video, about the stick's whole write budget. Like Linux's 30 s
/// dirty_expire, a hard kill can lose up to this much un-fsynced work.
const CP_INTERVAL_NS: u64 = 30_000_000_000;

fn maybe_flush(ms: &mut MountState) {
    // Counts the mutating requests since the last checkpoint. Checkpoints
    // themselves are decided outside the lock: `dispatch_locked` (dirty
    // pressure, writer throttle) and the background write-back (age).
    if ms.dirty_writes == 0 { ms.first_dirty_ns = sched::monotonic_ns(); }
    ms.dirty_writes = ms.dirty_writes.saturating_add(1);
    mark_changed(ms);
}

/// A namespace operation (create, mkdir, unlink, rmdir, symlink, link,
/// rename) completed. These used to checkpoint synchronously, every time:
/// each atomic config write (create temp + rename) cost two whole-cache
/// write-backs, and on the PC's USB stick COSMIC's config/state writes kept
/// the device busy with small writes for minutes.
///
/// Now it is committed by the next checkpoint: an fsync/sync, dirty
/// pressure, or the background write-back within `CP_INTERVAL_NS` + 1 s.
/// Crash consistency (the "?---------" torn-entry gap the synchronous
/// checkpoint closed) is kept: until that checkpoint NOTHING of the operation
/// is on the medium — the blocks it dirtied stay pinned in the cache, which
/// never evicts a dirty slot — so a hard kill falls back to the previous
/// checkpoint, whole. What a hard kill can lose is the last
/// `CP_INTERVAL_NS` of un-fsynced changes, as on Linux.
fn namespace_changed(ms: &mut MountState) {
    maybe_flush(ms);
    mark_changed_global(ms);
}

// ── Background write-back (lane f2fsperf) ─────────────────────────────────────
//
// A kernel task, started with the first mount, that sleeps until kicked (dirty
// pressure) or for 1 s, then checkpoints (unlocked) every read-write mount
// whose cache holds `soft_dirty` dirty blocks or whose oldest uncommitted
// change is `CP_INTERVAL_NS` old. Requests that dirty the cache never wait
// for a routine checkpoint: before, the request that crossed the threshold
// ran it in its own context (a Firefox thread could wait out a whole
// write-back on the stick).

static WB_KICK: sched::ksync::KSemaphore = sched::ksync::KSemaphore::new(0);
static WB_KICKED: atomic::AtomicBool = atomic::AtomicBool::new(false);
static WB_THREAD: atomic::AtomicBool = atomic::AtomicBool::new(false);

fn wb_thread() -> ! {
    loop {
        let _ = WB_KICK.down_timeout(1_000_000_000);
        WB_KICKED.store(false, atomic::Ordering::Relaxed);
        writeback_scan();
    }
}

fn start_wb_thread() {
    if WB_THREAD.swap(true, atomic::Ordering::AcqRel) { return; }
    if sched::spawn(wb_thread, 0).is_none() {
        WB_THREAD.store(false, atomic::Ordering::Release);
        dbg_str("[F2FS] could not start the write-back task; the I/O task does it\n");
    }
}

/// Ask for a checkpoint soon (dirty pressure). Never blocks.
fn kick_writeback(port: u32) {
    if WB_THREAD.load(atomic::Ordering::Acquire) {
        if !WB_KICKED.swap(true, atomic::Ordering::AcqRel) { WB_KICK.up(); }
    } else {
        let _ = checkpoint_unlocked(port, None, CpWhy::Pressure);
    }
}

fn writeback_scan() {
    let mut due = [(0u32, CpWhy::Age); MAX_MOUNTS];
    let mut n = 0;
    {
        let mounts = lock_mounts();
        let now = sched::monotonic_ns();
        for ms in mounts.iter().flatten() {
            let changed = ms.dirty_writes > 0 || ms.cache.ndirty > 0;
            let pressure = ms.cache.ndirty >= ms.cache.soft_dirty();
            let since = if ms.dirty_writes > 0 { ms.first_dirty_ns } else { ms.last_cp_ns };
            let old = now.saturating_sub(since) >= CP_INTERVAL_NS;
            // `f2fs.fsync=lazy`: a commit promised to fsyncs that returned
            // early is due (lane fsyncbatch).
            let lazy = cp_seq(ms.port).map_or(false, |s| {
                let d = s.lazy_due.load(atomic::Ordering::SeqCst);
                d != 0 && now >= d
            });
            if !ms.read_only && (lazy || changed && (pressure || old)) && n < due.len() {
                let why = if lazy { CpWhy::Lazy } else if pressure { CpWhy::Pressure } else { CpWhy::Age };
                due[n] = (ms.port, why);
                n += 1;
            }
        }
    }
    // The write-back itself runs without the filesystem lock (lane loginhang).
    for &(port, why) in &due[..n] { let _ = checkpoint_unlocked(port, None, why); }
}

/// Called every second by the kernel's I/O task: the write-back task's work,
/// if that task could not be started (it wakes every second on its own).
pub fn writeback_tick() {
    if !WB_THREAD.load(atomic::Ordering::Acquire) { writeback_scan(); }
}

// ── Inode operations ──────────────────────────────────────────────────────────

fn inode_size(blk: &[u8]) -> u64 { r64(blk, INO_SIZE) }
fn inode_mode(blk: &[u8]) -> u16 { r16(blk, INO_MODE) }
fn inode_uid(blk: &[u8]) -> u32 { r32(blk, INO_UID) }
fn inode_gid(blk: &[u8]) -> u32 { r32(blk, INO_GID) }
fn inode_links(blk: &[u8]) -> u32 { r32(blk, INO_LINKS) }
fn inode_is_dir(blk: &[u8]) -> bool { (inode_mode(blk) & S_IFMT) == S_IFDIR }

/// `(atime, mtime, ctime)` as `(sec, nsec)` pairs.
fn inode_times(blk: &[u8]) -> ((i64, i64), (i64, i64), (i64, i64)) {
    ((r64(blk, INO_ATIME) as i64, r32(blk, INO_ATIME_NS) as i64),
     (r64(blk, INO_MTIME) as i64, r32(blk, INO_MTIME_NS) as i64),
     (r64(blk, INO_CTIME) as i64, r32(blk, INO_CTIME_NS) as i64))
}
fn inode_set_atime(blk: &mut [u8], t: (i64, i64)) { w64(blk, INO_ATIME, t.0 as u64); w32(blk, INO_ATIME_NS, t.1 as u32); }
fn inode_set_mtime(blk: &mut [u8], t: (i64, i64)) { w64(blk, INO_MTIME, t.0 as u64); w32(blk, INO_MTIME_NS, t.1 as u32); }
fn inode_set_ctime(blk: &mut [u8], t: (i64, i64)) { w64(blk, INO_CTIME, t.0 as u64); w32(blk, INO_CTIME_NS, t.1 as u32); }
/// Data changed: mtime and ctime = now.
fn inode_touch_mtime(blk: &mut [u8]) { let n = sched::clock_ts(); inode_set_mtime(blk, n); inode_set_ctime(blk, n); }
/// Metadata changed: ctime = now, on the live inode block of `ino`.
fn touch_ctime(ms: &mut MountState, ino: u32) {
    mm::pagecache::note_ctime(page_cache_key(ms.port, ino));
    let addr = nat_lookup(ms, ino);
    if addr == 0 { return; }
    {
        let iblk = ms.cache.get_mut(ms.dev, addr as u64);
        inode_set_ctime(iblk, sched::clock_ts());
    }
    nat_update(ms, ino, addr);
}

/// Linux's `relatime` mount default (the option `/proc/mounts` already claims
/// for this filesystem): skip the atime update unless mtime or ctime is at or
/// past the current atime, or the current atime is at least a day stale.
/// Keeps a read from dirtying the inode block on every call.
const RELATIME_INTERVAL_SEC: i64 = 86_400;
fn relatime_needs_update(atime: (i64, i64), mtime: (i64, i64), ctime: (i64, i64), now: (i64, i64)) -> bool {
    if atime.0 < mtime.0 || (atime.0 == mtime.0 && atime.1 <= mtime.1) { return true; }
    if atime.0 < ctime.0 || (atime.0 == ctime.0 && atime.1 <= ctime.1) { return true; }
    now.0.saturating_sub(atime.0) >= RELATIME_INTERVAL_SEC
}

/// A read happened: relatime-gated atime touch on the live inode block of `ino`.
fn touch_atime_relatime(ms: &mut MountState, ino: u32) {
    if ms.read_only { return; }
    let addr = nat_lookup(ms, ino);
    if addr == 0 { return; }
    let now = sched::clock_ts();
    let needs = {
        let iblk = ms.cache.read(ms.dev, addr as u64);
        let (at, mt, ct) = inode_times(iblk);
        relatime_needs_update(at, mt, ct, now)
    };
    if !needs { return; }
    // lazytime (lane fsio): the new atime lives in the cached inode block,
    // which is NOT marked dirty. It reaches the medium with the next real
    // change of that inode (or is dropped if the block is evicted first).
    // relatime used to dirty the inode block AND its NAT block on the first
    // read of every file (an image built on the host has atime == mtime), so
    // a COSMIC session start queued hundreds of scattered in-place writes for
    // the next checkpoint — on the PC's USB stick, seconds of writes per
    // checkpoint with the filesystem lock held. stat() still sees the new
    // atime (it reads through the cache). The inode stays at `addr`, so the
    // NAT entry needs no update either.
    let iblk = ms.cache.get_clean_mut(ms.dev, addr as u64);
    inode_set_atime(iblk, now);
}

/// Blocks charged to this inode, or `None` when the inode predates the counter
/// (see `F2FS_ADVISE_IBLOCKS`).
fn inode_blocks(blk: &[u8]) -> Option<u64> {
    if blk[INO_ADVISE] & F2FS_ADVISE_IBLOCKS != 0 { Some(r64(blk, INO_BLOCKS)) } else { None }
}

/// `st_blocks` (512-byte units) for an inode, from the real count when it is
/// kept and from `i_size` otherwise. Saturating rather than wrapping: a bad
/// count must not report a file as astronomically large, and it must never
/// underflow to `u64::MAX`.
fn inode_stat_blocks(blk: &[u8]) -> u64 {
    match inode_blocks(blk) {
        Some(b) => b.saturating_mul((BLOCK_SIZE / 512) as u64),
        None    => inode_size(blk).div_ceil(512),
    }
}

/// Charge (`delta > 0`) or refund (`delta < 0`) blocks on an inode image.
/// No-op on an inode that does not carry the counter, so an old inode is never
/// given a half-maintained one.
fn inode_add_blocks(blk: &mut [u8], delta: i64) {
    if blk[INO_ADVISE] & F2FS_ADVISE_IBLOCKS == 0 { return; }
    let cur = r64(blk, INO_BLOCKS);
    let new = if delta >= 0 {
        cur.saturating_add(delta as u64)
    } else {
        cur.saturating_sub(delta.unsigned_abs())
    };
    w64(blk, INO_BLOCKS, new);
}

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
    // Opt this inode into real block accounting, starting at 1 for the inode
    // block itself — same convention as Linux F2FS, where i_blocks counts the
    // node blocks too.
    buf[INO_ADVISE] |= F2FS_ADVISE_IBLOCKS;
    w64(&mut buf, INO_BLOCKS,  1);
    let now = sched::clock_ts();
    inode_set_atime(&mut buf, now);
    inode_set_mtime(&mut buf, now);
    inode_set_ctime(&mut buf, now);
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
/// takes `cache.get_mut` on a SIT block, which can evict the very node block
/// being walked. Holding a `&` into the
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
    data_changed(ms, ino);
    // The inode is going away: so do its cached pages (a doomed round
    // dropping them early costs a re-read, nothing else).
    mm::pagecache::drop_file(page_cache_key(ms.port, ino));
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
    data_changed(ms, ino);
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
    let mut freed = 0u64;
    for i in 0..max_direct {
        if i as u64 >= keep {
            if free_block(ms, inode_get_blkaddr(&iblk, i)) { freed += 1; }
            inode_set_blkaddr(&mut iblk, i, 0);
        }
    }
    let mut base = max_direct as u64;

    // i_nid[0], i_nid[1]: direct nodes.
    for slot in 0..=1usize {
        let nid = inode_get_nid(&iblk, slot);
        if truncate_dnode(ms, nid, base, keep, &mut freed) { inode_set_nid(&mut iblk, slot, 0); }
        base += ADDRS;
    }

    // i_nid[2], i_nid[3]: single-indirect.
    let per_ind = NIDS * ADDRS;
    for slot in 2..=3usize {
        let nid = inode_get_nid(&iblk, slot);
        if truncate_indirect(ms, nid, base, keep, &mut freed) { inode_set_nid(&mut iblk, slot, 0); }
        base += per_ind;
    }

    // i_nid[4]: double-indirect.
    let dind_nid = inode_get_nid(&iblk, 4);
    if truncate_dindirect(ms, dind_nid, base, keep, &mut freed) { inode_set_nid(&mut iblk, 4, 0); }

    inode_add_blocks(&mut iblk, -(freed as i64));
    w64(&mut iblk, INO_SIZE, new_len);
    ms.cache.write(ms.dev, iblkaddr as u64, &iblk);
    nat_update(ms, ino, iblkaddr);
    // Pages past the new end leave the page cache, the new last page's tail
    // reads zeros — from the round that completes.
    if !ms.cache.poisoned() { mm::pagecache::truncate(page_cache_key(ms.port, ino), new_len); }
}

/// Free the data blocks of direct node `nid` whose logical index is `>= keep`
/// (its slots cover indices `[base, base + 1019)`). Returns `true` when the
/// node block itself was freed because nothing live remained, so the caller
/// must clear its pointer.
fn truncate_dnode(ms: &mut MountState, nid: u32, base: u64, keep: u64, freed: &mut u64) -> bool {
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
            if free_block(ms, dnode_get_blkaddr(&dblk, i)) { *freed += 1; }
            dnode_set_blkaddr(&mut dblk, i, 0);
        } else if dnode_get_blkaddr(&dblk, i) != 0 {
            any_kept = true;
        }
    }
    if any_kept {
        ms.cache.write(ms.dev, dblkaddr as u64, &dblk);
        false
    } else {
        if free_block(ms, dblkaddr) { *freed += 1; }
        true
    }
}

/// One level up from `truncate_dnode`: an indirect node holding up to 1019
/// direct-node nids, each covering 1019 logical indices.
fn truncate_indirect(ms: &mut MountState, nid: u32, base: u64, keep: u64, freed: &mut u64) -> bool {
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
        if truncate_dnode(ms, child_nid, child_base, keep, freed) {
            dnode_set_blkaddr(&mut ind_blk, i, 0);
        } else {
            any_kept = true;
        }
    }
    if any_kept {
        ms.cache.write(ms.dev, ind_blkaddr as u64, &ind_blk);
        false
    } else {
        if free_block(ms, ind_blkaddr) { *freed += 1; }
        true
    }
}

/// One more level up: a double-indirect node holding up to 1019 indirect-node
/// nids.
fn truncate_dindirect(ms: &mut MountState, nid: u32, base: u64, keep: u64, freed: &mut u64) -> bool {
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
        if truncate_indirect(ms, child_nid, child_base, keep, freed) {
            dnode_set_blkaddr(&mut dind_blk, i, 0);
        } else {
            any_kept = true;
        }
    }
    if any_kept {
        ms.cache.write(ms.dev, dind_blkaddr as u64, &dind_blk);
        false
    } else {
        if free_block(ms, dind_blkaddr) { *freed += 1; }
        true
    }
}

/// Read `count` bytes from file `ino` at `pos` into `buf`.
fn read_file_data(ms: &mut MountState, ino: u32, pos: u64, buf: *mut u8, count: usize) -> usize {
    read_file_data_planned(ms, ino, pos, buf, count, None)
}

/// [`read_file_data`], or — with `plan` — the same walk that copies what the
/// block cache has and zero-fills holes, but RECORDS the uncached,
/// physically contiguous full-block runs in `plan` instead of reading them
/// (the caller reads them with the lock dropped; see `read_unlocked`).
/// Returns the byte count the read covers either way.
fn read_file_data_planned(ms: &mut MountState, ino: u32, pos: u64, buf: *mut u8, count: usize,
                          mut plan: Option<&mut alloc::vec::Vec<Run>>) -> usize {
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
        // multi-block request straight into the destination buffer.
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
            if let Some(p) = plan.as_deref_mut() {
                p.push(Run { off: done, phys, nblk: run });
                done += run * BLOCK_SIZE;
                continue;
            }
            let dst = unsafe {
                core::slice::from_raw_parts_mut(buf.add(done), run * BLOCK_SIZE)
            };
            if ms.cache.probe != PROBE_OFF {
                // No device I/O with the lock held (lane nostall): the run is
                // this request's miss, fetched into the cache unlocked.
                ms.cache.note_miss(phys as u64, run);
                dst.fill(0);
                done += run * BLOCK_SIZE;
                continue;
            }
            if io_read_blocks(IO_READ_DIRECT, ms.dev, phys as u64, dst) {
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

/// Read `len` bytes at `pos` of the file `resolve` names on the mount at
/// `port`, with every device read done OUTSIDE the mount-table lock (lane
/// fsio; lane nostall made it every read, small ones included). Under the
/// lock (`with_mount`, so metadata misses are fetched unlocked too): resolve
/// the inode, copy cached blocks, zero-fill holes, and plan the uncached
/// runs. Then drop the lock, read the runs into `dst`, and check the file's
/// data generation: unchanged means no write, truncate or free touched its
/// block map since the plan, so the bytes are exactly what a locked read
/// would have returned; changed (or a device error, or - for read(2) - the
/// descriptor's offset moved meanwhile: `finish` returns false) plans again,
/// and after
/// `UNLOCKED_READ_TRIES` reads the file through the cache. `finish`, if any,
/// runs under the lock once the result is final (VFS_READ's position update
/// and atime). Returns the byte count or a negative errno.
fn read_unlocked(port: u32, dst: *mut u8, len: usize,
                 resolve: &dyn Fn(&MountState) -> Result<(u32, u64), isize>,
                 finish: Option<&dyn Fn(&mut MountState, u32, u64, usize) -> bool>) -> isize {
    enum Plan { Done(isize), Io { n: usize, dev: usize, ino: u32, pos: u64, gen: u32 } }
    let mut runs: alloc::vec::Vec<Run> = alloc::vec::Vec::new();
    for _ in 0..UNLOCKED_READ_TRIES {
        let plan = with_mount(port, |ms| {
            act_code(VFS_READ);
            runs.clear();
            let (ino, pos) = match resolve(ms) { Ok(r) => r, Err(e) => return Plan::Done(e) };
            let gen = gen_slot(port, ino).load(atomic::Ordering::Acquire);
            let n = read_file_data_planned(ms, ino, pos, dst, len, Some(&mut runs));
            if runs.is_empty() {
                if let Some(f) = finish { f(ms, ino, pos, n); }
                return Plan::Done(n as isize);
            }
            Plan::Io { n, dev: ms.dev, ino, pos, gen }
        });
        let (n, dev, ino, pos, gen) = match plan {
            None => return -9, // EBADF — no mount on this port
            Some(Plan::Done(r)) => return r,
            Some(Plan::Io { n, dev, ino, pos, gen }) => (n, dev, ino, pos, gen),
        };
        let ok = runs.iter().all(|r| virtio_blk::read_blocks(dev, r.phys as u64,
            unsafe { core::slice::from_raw_parts_mut(dst.add(r.off), r.nblk * BLOCK_SIZE) }));
        let fresh = ok && gen_slot(port, ino).load(atomic::Ordering::Acquire) == gen;
        if fresh {
            let Some(f) = finish else { return n as isize };
            // The position update and atime, if the file (and, for read(2),
            // the descriptor's offset) is still as planned: read(2) on a
            // shared descriptor stays atomic with respect to the offset.
            match with_mount(port, |ms| {
                if gen_slot(port, ino).load(atomic::Ordering::Acquire) != gen { return false; }
                f(ms, ino, pos, n)
            }) {
                None => return -9,
                Some(true) => return n as isize,
                Some(false) => {}
            }
        }
        UNLOCKED_REDOS.fetch_add(1, atomic::Ordering::Relaxed);
    }
    // The file kept changing under the unlocked reads (or the device kept
    // failing): read it through the block cache, still with the misses
    // fetched unlocked.
    with_mount(port, |ms| {
        act_code(VFS_READ);
        let (ino, pos) = match resolve(ms) { Ok(r) => r, Err(e) => return e };
        let n = read_file_data(ms, ino, pos, dst, len);
        if let Some(f) = finish { f(ms, ino, pos, n); }
        n as isize
    }).unwrap_or(-9)
}

/// Lock-free reads that had to be redone under the lock (diagnostic).
static UNLOCKED_REDOS: atomic::AtomicU64 = atomic::AtomicU64::new(0);

/// Write `count` bytes from `src` into file `ino` at `pos`.
fn write_file_data(ms: &mut MountState, ino: u32, pos: u64, src: *const u8, count: usize) -> usize {
    // Lock-free reads of this file in flight must be redone.
    data_changed(ms, ino);
    let iblkaddr = nat_lookup(ms, ino);
    let mut iblk_copy = {
        let b = ms.cache.read(ms.dev, iblkaddr as u64);
        let mut c = [0u8; BLOCK_SIZE]; c.copy_from_slice(b); c
    };
    let old_size = inode_size(&iblk_copy);

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

    // Update file size and the modification time (the inode block is being
    // rewritten anyway, so the stamp is free).
    let new_size = (pos + done as u64).max(inode_size(&iblk_copy));
    w64(&mut iblk_copy, INO_SIZE, new_size);
    inode_touch_mtime(&mut iblk_copy);
    ms.cache.write(ms.dev, iblkaddr as u64, &iblk_copy);
    nat_update(ms, ino, iblkaddr);

    maybe_flush(ms);
    // The kernel page cache gets the same bytes, from the round that
    // completes (still under the lock, so writes reach it in order).
    if !ms.cache.poisoned() {
        pc_write_through(ms.port, ino, pos, src, done, old_size, new_size);
    }
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
            inode_add_blocks(iblk, 1);
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
            inode_add_blocks(iblk, 1);
            nid = new_nid;
            inode_set_nid(iblk, 0, nid);
            ms.cache.write(ms.dev, iblkaddr as u64, iblk);
        }
        let dnblkaddr = nat_lookup(ms, nid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, rem as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            inode_add_blocks(iblk, 1);
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
            inode_add_blocks(iblk, 1);
            nid = new_nid;
            inode_set_nid(iblk, 1, nid);
            ms.cache.write(ms.dev, iblkaddr as u64, iblk);
        }
        let dnblkaddr = nat_lookup(ms, nid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, rem as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            inode_add_blocks(iblk, 1);
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
            inode_add_blocks(iblk, 1);
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
            inode_add_blocks(iblk, 1);
            dnid = new_nid;
            dnode_set_blkaddr(&mut ind_blk, dnode_idx as usize, dnid);
            ms.cache.write(ms.dev, ind_blkaddr as u64, &ind_blk);
        }
        let dnblkaddr = nat_lookup(ms, dnid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, dnode_off as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            inode_add_blocks(iblk, 1);
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
            inode_add_blocks(iblk, 1);
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
            inode_add_blocks(iblk, 1);
            dnid = new_nid;
            dnode_set_blkaddr(&mut ind_blk, dnode_idx as usize, dnid);
            ms.cache.write(ms.dev, ind_blkaddr as u64, &ind_blk);
        }
        let dnblkaddr = nat_lookup(ms, dnid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, dnode_off as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            inode_add_blocks(iblk, 1);
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
            inode_add_blocks(iblk, 1);
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
            inode_add_blocks(iblk, 1);
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
            inode_add_blocks(iblk, 1);
            dnid = new_nid;
            dnode_set_blkaddr(&mut ind_blk, dnode_idx as usize, dnid);
            ms.cache.write(ms.dev, ind_blkaddr as u64, &ind_blk);
        }
        let dnblkaddr = nat_lookup(ms, dnid);
        let mut dnblk_copy = *ms.cache.read(ms.dev, dnblkaddr as u64);
        let phys = dnode_get_blkaddr(&dnblk_copy, dnode_off as usize);
        if phys != 0 { return phys; }
        if let Some(new_phys) = alloc_data_block(ms) {
            inode_add_blocks(iblk, 1);
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
        if ms.cache.poisoned() { break; }
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
        if ms.cache.poisoned() { break; }
        let phys = if blk_pass < n_data_blks {
            let p = inode_logical_to_phys(ms, dir_ino, blk_pass as u64);
            if p == 0 { continue; }
            p
        } else {
            // Allocate a new data block
            // `alloc_data_block` hands the block back already zeroed, which a
            // dentry block relies on: every slot must read as "free" and every
            // name byte as NUL.
            let p = match alloc_data_block(ms) { Some(p) => p, None => return false };
            // Update inode
            inode_add_blocks(&mut iblk_copy, 1);
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
        if ms.cache.poisoned() { break; }
        let phys = inode_logical_to_phys(ms, dir_ino, blk_idx as u64);
        if phys == 0 { continue; }
        // Scan a copy and dirty only the block that holds the entry (every
        // scanned block used to be dirtied: one more scattered write each at
        // the next checkpoint).
        let dblk = *ms.cache.read(ms.dev, phys as u64);
        let mut slot = 0usize;
        while slot < NR_DENTRY_IN_BLK {
            let byte = slot / 8;
            let bit  = slot % 8;
            if byte >= DENTRY_BITMAP_SIZE { break; }
            if (dblk[byte] & (1 << bit)) == 0 { slot += 1; continue; }
            let e_off    = DENTRY_ENTRIES_OFF + slot * DENTRY_ENTRY_SIZE;
            let name_len = r16(&dblk, e_off + 8) as usize;
            let n_off    = DENTRY_NAMES_OFF + slot * DENTRY_SLOT_LEN;
            if name_len == name.len()
                && n_off + name_len <= BLOCK_SIZE
                && &dblk[n_off..n_off + name_len] == name
            {
                let slots_used = (name_len + DENTRY_SLOT_LEN - 1) / DENTRY_SLOT_LEN;
                let wblk = ms.cache.get_mut(ms.dev, phys as u64);
                for s in slot..slot + slots_used {
                    let b = s / 8; let bit = s % 8;
                    if b < DENTRY_BITMAP_SIZE { wblk[b] &= !(1 << bit); }
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

// ── permission gates ─────────────────────────────────────────────────────────
//
// Every gate funnels through `xattr::may_access` (mode bits + stored POSIX ACL
// + root bypass) on the inode's live meta. `ms.cred` is the caller's,
// published by `dispatch_msg`. Errors are POSIX: a component the caller may not
// search is EACCES even when what follows it does not exist, so a walk never
// leaks existence through ENOENT.

/// `xattr::may_access` on inode `ino` for the current caller. Reads the xattr
/// node only when the inode has one, so the common ACL-less directory costs a
/// single (cached) inode-block read.
fn may_access_ino(ms: &mut MountState, ino: u32, mask: u8) -> bool {
    let cred = ms.cred;
    let (meta, xnid) = load_meta_xnid(ms, ino);
    if cred.euid == 0 && !(mask & xattr::MAY_EXEC != 0 && !xattr::is_dir(meta.mode)) {
        return true; // root: skip the xattr read entirely
    }
    let xbuf = if xnid != 0 { Some(read_xattr_arena(ms, xnid)) } else { None };
    let acl = match &xbuf {
        Some(b) => xattr::find(&b[..xattr::F2FS_XATTR_ARENA], xattr::IDX_ACL_ACCESS, b""),
        None => None,
    };
    xattr::may_access(&meta, &cred, acl, mask)
}

/// Gate for creating an entry in / removing an entry from directory `dir`:
/// write + search on the directory. `Err(-EACCES)` when refused.
fn may_modify_dir(ms: &mut MountState, dir: u32) -> Result<(), i32> {
    if may_access_ino(ms, dir, xattr::MAY_WRITE | xattr::MAY_EXEC) { Ok(()) } else { Err(-13) }
}

/// Gate for creating a NEW entry `name` in directory `parent`, in Linux's
/// order (`filename_create` then `may_create`): search on the parent
/// (EACCES), the existence probe (EEXIST), then write + search on the parent
/// (EACCES). The order is observable — `mkdir /usr` as a user is "File
/// exists" — and Rust's `create_dir_all` depends on it: it stops at the
/// first error that is not NotFound unless the path already is a directory,
/// and only asks that question after EEXIST, never after EACCES. So every
/// `create_dir_all` over a chain some other uid already made (the session's
/// `/run/cosmic-greeter/...`, seeded by the root greeter compositor) failed
/// with the old order. Existence is no secret to a caller with search
/// permission anyway; `stat` answers it.
fn create_gate(ms: &mut MountState, parent: u32, name: &[u8]) -> Result<(), i32> {
    if !may_access_ino(ms, parent, xattr::MAY_EXEC) { return Err(-13); }
    if dir_lookup(ms, parent, name) != 0 { return Err(-17); } // EEXIST
    may_modify_dir(ms, parent)
}

/// Removal/replacement of `victim` inside `dir`: `may_modify_dir` plus the
/// sticky-bit rule (only the entry's owner, the directory's owner or root may
/// remove an entry from a directory with S_ISVTX set — EPERM otherwise).
fn may_delete_in(ms: &mut MountState, dir: u32, victim: u32) -> Result<(), i32> {
    may_modify_dir(ms, dir)?;
    let (dmeta, _) = load_meta_xnid(ms, dir);
    let (vmeta, _) = load_meta_xnid(ms, victim);
    if xattr::sticky_denies(&dmeta, vmeta.uid, ms.cred.euid) { Err(-1) } else { Ok(()) }
}

/// Resolve the parent of a to-be-created / to-be-removed entry. `parent_rel`
/// is the volume-relative parent path ("" or "/" for the root).
fn resolve_parent(ms: &mut MountState, parent_rel: &[u8]) -> Result<u32, i32> {
    if parent_rel.is_empty() || parent_rel == b"/" {
        return Ok(ms.sb.root_ino);
    }
    resolve_path_r(ms, parent_rel, true)
}

/// `resolve_path_ex` with the errno preserved: `Err(-ENOENT)` for a missing
/// component, `Err(-EACCES)` for a directory the caller may not search,
/// `Err(-ENOTDIR)` for a non-directory used as one, `Err(-ELOOP)` on a
/// symlink cycle.
fn resolve_path_r(ms: &mut MountState, path: &[u8], follow_final: bool) -> Result<u32, i32> {
    match resolve_path_ex(ms, path, follow_final) {
        0 => Err(ms.take_walk_err()),
        ino => Ok(ino),
    }
}

impl MountState {
    /// The errno of the last failed `resolve_path_ex` walk, defaulting to
    /// ENOENT. Cleared on read so a stale value can never outlive its walk.
    fn take_walk_err(&mut self) -> i32 {
        let e = self.walk_err;
        self.walk_err = 0;
        if e == 0 { -2 } else { e }
    }
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

    ms.walk_err = 0;
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

            // Search permission on the directory we are about to look into.
            // This is the traversal check POSIX requires on every component of
            // the prefix, and it comes BEFORE the lookup so an unsearchable
            // directory answers EACCES whether or not the name exists in it.
            // A non-directory in the middle of a path is ENOTDIR.
            {
                let is_dir = {
                    let addr = nat_lookup(ms, ino);
                    let iblk = ms.cache.read(ms.dev, addr as u64);
                    inode_is_dir(iblk)
                };
                if !is_dir { ms.walk_err = -20; return 0; } // ENOTDIR
                if !may_access_ino(ms, ino, xattr::MAY_EXEC) {
                    ms.walk_err = -13; // EACCES
                    return 0;
                }
            }

            let (next, ftype) = dir_lookup_ft(ms, ino, &name[..nlen]);
            if next == 0 { return 0; }

            let is_last = comp_end == len;
            if ftype == DT_LNK && !(is_last && !follow_final) {
                hops += 1;
                if hops > SYMLINK_MAX_HOPS { ms.walk_err = -40; return 0; } // ELOOP

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
                    if jlen > 1 {
                        // Jailed: an absolute body is taken relative to the jail
                        // root — exactly as the kernel prefixes explicit paths
                        // for a jailed task — so splice the jail root, then the
                        // body verbatim. `floor` stops `..` climbing out.
                        push(&jail[..jlen], &mut n);
                        push(&target[..tlen], &mut n);
                    } else {
                        // Unjailed: the body is a full, mount-prefixed path
                        // (`/data/x`), but this walk runs in volume-relative
                        // coordinates. Strip this volume's mount prefix to
                        // translate it — the same transform `get_relative_path`
                        // applies to an incoming path — so a link back through
                        // the mount point (`ln -s /data/x l` inside /data) is
                        // found instead of walking a phantom `/data/data/x`. A
                        // body naming another mount fails to strip to this volume
                        // and resolves within it (ENOENT), never an escape:
                        // cross-mount targets out of f2fs are the documented
                        // limitation above.
                        let rel = get_relative_path(ms, &target[..tlen])
                            .unwrap_or(&target[..tlen]);
                        push(rel, &mut n);
                    }
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
               cred: &xattr::Cred) -> Message {
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
    if ms.read_only && (writable || (flags & O_TRUNC) != 0) {
        return err_reply(-30); // EROFS
    }

    // With O_NOFOLLOW the final component must not be traversed, so that a
    // symlink resolves to *itself* and can then be rejected below. Resolving
    // first and checking afterwards would already have opened the target.
    let ino = match resolve_path_r(ms, rel, !nofollow) {
        Ok(i) => i,
        // A prefix the caller may not search (or that is not a directory) is
        // the answer regardless of O_CREAT: only a genuinely missing final
        // component may fall through to creation.
        Err(e) if e != -2 => return err_reply(e),
        Err(_) => 0,
    };

    let ino = if ino == 0 {
        if !create || !writable { return err_reply(-2); } // ENOENT
        // Create the file
        let (parent_path, name) = path_split(rel);
        let parent_ino = match resolve_parent(ms, parent_path) {
            Ok(p) => p,
            Err(e) => return err_reply(e),
        };
        // Creating an entry needs write + search on the parent directory.
        if let Err(e) = may_modify_dir(ms, parent_ino) { return err_reply(e); }
        // The caller's mode, not a hardcoded 0644. The umask travels with it
        // (`xattr::pack_create_mode`) and `acl_create` applies it — or the
        // parent's default ACL instead, when there is one — through the same
        // function tmpfs uses, so the two filesystems cannot disagree.
        let (m, um) = xattr::unpack_create_mode(mode);
        let created = inherit_create_mode(ms, parent_ino, m, um, false);
        let imode = S_IFREG | (created.mode & 0o7777);
        let (new_ino, _) = match create_inode(ms, imode, cred.euid, cred.egid, parent_ino, name) {
            Some(v) => v,
            None    => return err_reply(-28), // ENOSPC
        };
        store_inherited_acls(ms, new_ino, parent_ino, &created);
        if !dir_add_entry(ms, parent_ino, name, new_ino, DT_REG) {
            return err_reply(-28);
        }
        // Namespace mutation complete (inode + dentry). Commit point (see
        // namespace_changed: deferred, but a hard kill still never exposes a
        // torn/half-written new file); the open-file slot set up below is
        // in-RAM only and needs no persistence.
        namespace_changed(ms);
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
        if cred.euid != 0 {
            let want_read  = flags & O_WRONLY == 0; // RDONLY/RDWR read; WRONLY does not
            // O_TRUNC destroys content, so it needs write permission even on
            // an O_RDONLY open (Linux may_open: O_TRUNC implies MAY_WRITE).
            let want_write = writable || flags & O_TRUNC != 0;
            let (meta, xnid) = load_meta_xnid(ms, ino);
            let xbuf = if xnid != 0 { Some(read_xattr_arena(ms, xnid)) } else { None };
            let acl = match &xbuf {
                Some(b) => xattr::find(&b[..xattr::F2FS_XATTR_ARENA], xattr::IDX_ACL_ACCESS, b""),
                None => None,
            };
            if !xattr::access_check(&meta, cred, acl, want_read, want_write, false) {
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
        None    => { report_open_files_full(); return err_reply(-24); } // EMFILE
    };
    if !of_log(ms, slot) { return err_reply(-5); } // doomed request: discarded
    ms.open_files[slot] = OpenFile { inode: ino, writable, in_use: true,
                                     path: [0; MAX_OPEN_PATH], path_len: 0 };
    if path_bytes.len() <= MAX_OPEN_PATH {
        ms.open_files[slot].path[..path_bytes.len()].copy_from_slice(path_bytes);
        ms.open_files[slot].path_len = path_bytes.len();
    }
    if !ms.cache.poisoned() { slot_opened(ms.port, slot, ino); }
    val_reply(slot as u64)
}

// read(2), write(2) and lseek(2) on a mounted file are served by
// `f2fs_dispatch` (offset in `SLOT_FAST`, data through the page cache).

fn handle_close(ms: &mut MountState, file_id: u64) -> Message {
    let slot = file_id as usize;
    if slot < MAX_OPEN_FILES && of_log(ms, slot) {
        let was_open = ms.open_files[slot].in_use;
        let ino = ms.open_files[slot].inode;
        ms.open_files[slot].in_use = false;
        if !ms.cache.poisoned() {
            slot_closed(ms.port, slot);
            // The last close of a file whose last name is gone: its pages
            // will never be read again.
            let key = page_cache_key(ms.port, ino);
            if was_open && mm::pagecache::is_orphan(key) && !ino_is_open(ms, ino) {
                mm::pagecache::drop_file(key);
            }
        }
    }
    ok_reply()
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
    let blks  = inode_stat_blocks(iblk);
    let (at, mt, ct) = inode_times(iblk);
    vfs_server::write_stat_full_blocks(stat_ptr as usize, mode, links as u64, size,
                                       ino as u64, uid, gid, blks);
    vfs_server::write_stat_times(stat_ptr as usize, at, mt, ct);
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
    let ino = match resolve_path_r(ms, rel, follow) { Ok(i) => i, Err(e) => return err_reply(e) };

    let iblkaddr = nat_lookup(ms, ino);
    let iblk = ms.cache.read(ms.dev, iblkaddr as u64);
    let mode  = inode_mode(iblk) as u32;
    let size  = inode_size(iblk);
    let links = inode_links(iblk);
    let uid   = inode_uid(iblk);
    let gid   = inode_gid(iblk);
    let blks  = inode_stat_blocks(iblk);
    let (at, mt, ct) = inode_times(iblk);

    // Emit the stat struct in the target's native layout. This used to
    // open-code the x86-64 offsets, which put st_mode and st_nlink in the
    // wrong slots on AArch64 (the two fields swap places there) and wrote
    // 144 bytes into a 128-byte buffer. `mode` here is the on-disk i_mode,
    // so it already carries the real type + permission bits (0o100755 for
    // the /bin binaries), and `links` the real hard-link count. `uid`/`gid`
    // used to be hardcoded 0/0 since nothing ever persisted a chown — now
    // that handle_chown writes INO_UID/INO_GID, stat reflects it.
    vfs_server::write_stat_full_blocks(stat_ptr as usize, mode, links as u64, size,
                                       ino as u64, uid, gid, blks);
    vfs_server::write_stat_times(stat_ptr as usize, at, mt, ct);
    ok_reply()
}

/// getdents64 at directory offset `current_pos` (entry index); returns the
/// reply and the offset to leave the descriptor at.
fn handle_getdents(ms: &mut MountState, file_id: u64, buf_ptr: u64, count: u64,
                   current_pos: u64) -> (Message, u64) {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return (err_reply(-9), current_pos); }
    let dir_ino = ms.open_files[slot].inode;
    let iblkaddr = nat_lookup(ms, dir_ino);
    let iblk_copy = {
        let b = ms.cache.read(ms.dev, iblkaddr as u64);
        let mut c = [0u8; BLOCK_SIZE]; c.copy_from_slice(b); c
    };
    if !inode_is_dir(&iblk_copy) { return (err_reply(-20), current_pos); } // ENOTDIR

    let fsize = inode_size(&iblk_copy);
    let n_data_blks = fsize.div_ceil(BLOCK_SIZE as u64) as usize;

    let mut written = 0usize;
    let max = count as usize;
    let mut entry_idx = 0u64;

    // linux_dirent64 layout: ino(8)+off(8)+reclen(2)+type(1)+name(var)+null(1), padded to 8
    for blk_idx in 0..n_data_blks {
        if ms.cache.poisoned() { break; }
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
                    return (val_reply(written as u64), entry_idx);
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
    (val_reply(written as u64), entry_idx)
}

fn handle_mkdir(ms: &mut MountState, path_ptr: u64, mode: u64,
                cred: &xattr::Cred) -> Message {
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
    let parent_ino = match resolve_parent(ms, parent_rel) {
        Ok(p) => p,
        Err(e) => return err_reply(e),
    };
    // Check parent is a directory
    let parent_iblkaddr = nat_lookup(ms, parent_ino);
    {
        let iblk = ms.cache.read(ms.dev, parent_iblkaddr as u64);
        if !inode_is_dir(iblk) { return err_reply(-20); }
    }
    // Search on the parent, EEXIST, then write on the parent — Linux's order.
    if let Err(e) = create_gate(ms, parent_ino, name) { return err_reply(e); }

    let (m, um) = xattr::unpack_create_mode(mode);
    let created = inherit_create_mode(ms, parent_ino, m, um, true);
    let imode = S_IFDIR | (created.mode & 0o7777);
    let (new_ino, _) = match create_inode(ms, imode, cred.euid, cred.egid, parent_ino, name) {
        Some(v) => v,
        None    => return err_reply(-28),
    };
    store_inherited_acls(ms, new_ino, parent_ino, &created);
    if !dir_add_entry(ms, parent_ino, name, new_ino, DT_DIR) {
        return err_reply(-28);
    }
    // Add "." and ".." entries to the new directory
    dir_add_entry(ms, new_ino, b".", new_ino, DT_DIR);
    dir_add_entry(ms, new_ino, b"..", parent_ino, DT_DIR);
    // Commit point (namespace_changed: deferred, crash-consistent): a hard kill (the
    // QEMU/driver workflow never runs a clean unmount) can never expose a torn
    // directory — the ?--------- crash-consistency gap. The single-threaded
    // server's in-RAM state is consistent between requests, so a checkpoint here
    // writes the child inode, both dentry blocks, NAT and next_free_nid together.
    namespace_changed(ms);
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
    let parent_ino = match resolve_parent(ms, parent_rel) {
        Ok(p) => p,
        Err(e) => return err_reply(e),
    };
    // Search on the parent is what makes the lookup below legitimate; write on
    // it is what unlink itself needs. Checked before the lookup so a caller
    // without them learns nothing about the directory's contents.
    if !may_access_ino(ms, parent_ino, xattr::MAY_EXEC) { return err_reply(-13); }
    let ino = dir_lookup(ms, parent_ino, name);
    if ino == 0 { return err_reply(-2); }
    let iblkaddr = nat_lookup(ms, ino);
    let is_dir = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_is_dir(iblk) };
    if is_dir { return err_reply(-21); } // EISDIR — use rmdir() instead
    // Write on the parent, plus the sticky rule for a directory like /tmp.
    if let Err(e) = may_delete_in(ms, parent_ino, ino) { return err_reply(e); }
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
        // Its cached pages serve the open descriptors until the last close.
        mm::pagecache::set_orphan(page_cache_key(ms.port, ino));
    } else {
        // Last link, not open: reclaim for real. Data blocks and the whole
        // node tree first, then the inode block itself.
        free_inode_data_and_nodes(ms, ino);
        free_block(ms, iblkaddr);
    }
    // Commit point (deferred, crash-consistent) — see namespace_changed. Keeps a hard kill from
    // tearing the dentry-removal / link-count / reclaim across a stale checkpoint.
    namespace_changed(ms);
    ok_reply()
}

/// True if any open descriptor on this mount still names `ino`.
///
/// Reclaim must skip an inode that is open — see the unlink path. Cheap linear
/// scan; `MAX_OPEN_FILES` is small.
fn ino_is_open(ms: &MountState, ino: u32) -> bool {
    ms.open_files.iter().any(|f| f.in_use && f.inode == ino)
        || MMAP_PINS.lock().iter().any(|&(port, i)| port == ms.port && i == ino)
}

/// Inodes pinned by the kernel's private file mappings (`pin_inode`): a
/// mapping outlives the descriptor it was made from, and its pages are read
/// on first touch, so the inode must survive an unlink/rename-over exactly as
/// an open descriptor makes it. One entry per mapped inode (the kernel
/// dedups and refcounts); lock order: taken inside F2FS_MOUNTS, never around it.
static MMAP_PINS: Mutex<alloc::vec::Vec<(u32, u32)>> = Mutex::new(alloc::vec::Vec::new());

/// Key of inode `ino` of the mount on `port` in the kernel's page cache
/// (`mm::pagecache`): every data change of the inode is applied to it.
pub fn page_cache_key(port: u32, ino: u32) -> u64 {
    ((port as u64 + 1) << 32) | ino as u64
}

/// Pin inode `ino` of the mount on `port` against reclaim (see MMAP_PINS).
pub fn pin_inode(port: u32, ino: u32) {
    MMAP_PINS.lock().push((port, ino));
}

/// Drop one pin taken by `pin_inode`.
pub fn unpin_inode(port: u32, ino: u32) {
    let mut pins = MMAP_PINS.lock();
    if let Some(i) = pins.iter().position(|&(p, n)| p == port && n == ino) {
        pins.swap_remove(i);
    }
}

/// symlink(target, linkpath) — create a symlink inode holding `target` as its
/// file data.
///
/// The volume is built with `^inline_data`, so even a two-byte target costs a
/// full data block. That matches how every other file on this volume is
/// stored and keeps the read path (`read_file_data`) the single one.
fn handle_symlink(ms: &mut MountState, target_ptr: u64, link_ptr: u64,
                  cred: &xattr::Cred) -> Message {
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
    let parent_ino = match resolve_parent(ms, parent_rel) {
        Ok(p) => p,
        Err(e) => return err_reply(e),
    };
    if let Err(e) = create_gate(ms, parent_ino, name) { return err_reply(e); }

    // Copy the target off the caller's buffer before any allocation runs: the
    // pointer is only guaranteed live for the duration of this call, and the
    // block writes below can flush and re-enter the cache.
    let mut tbuf = [0u8; 256];
    let tlen = target.len().min(255);
    tbuf[..tlen].copy_from_slice(&target[..tlen]);

    let (new_ino, _) = match create_inode(ms, S_IFLNK | 0o777, cred.euid, cred.egid, parent_ino, name) {
        Some(v) => v,
        None    => return err_reply(-28), // ENOSPC
    };
    if write_file_data(ms, new_ino, 0, tbuf.as_ptr(), tlen) != tlen {
        return err_reply(-28); // ENOSPC
    }
    if !dir_add_entry(ms, parent_ino, name, new_ino, DT_LNK) {
        return err_reply(-28);
    }
    // Commit point (deferred, crash-consistent) — see namespace_changed.
    namespace_changed(ms);
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
    let ino = match resolve_path_r(ms, rel, false) { Ok(i) => i, Err(e) => return err_reply(e) };

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
    let src_ino = match resolve_path_r(ms, old_rel, false) {
        Ok(i) => i,
        Err(e) => return err_reply(e),
    };

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
    let parent_ino = match resolve_parent(ms, parent_rel) {
        Ok(p) => p,
        Err(e) => return err_reply(e),
    };
    if let Err(e) = create_gate(ms, parent_ino, name) { return err_reply(e); }
    // protected_hardlinks (xattr::hardlink_denied): Linux `do_linkat` runs
    // `may_linkat` after `filename_create`, i.e. after the new name's checks.
    {
        let (smeta, _) = load_meta_xnid(ms, src_ino);
        let euid = ms.cred.euid;
        let may_rw = euid == 0 || euid == smeta.uid
            || may_access_ino(ms, src_ino, xattr::MAY_READ | xattr::MAY_WRITE);
        if xattr::hardlink_denied(&smeta, euid, may_rw) { return err_reply(-1); } // EPERM
    }

    let ftype = match mode & S_IFMT {
        S_IFLNK => DT_LNK,
        _       => DT_REG,
    };
    if !dir_add_entry(ms, parent_ino, name, src_ino, ftype) { return err_reply(-28); }

    let iblk = ms.cache.get_mut(ms.dev, addr as u64);
    w32(iblk, INO_LINKS, links + 1);
    nat_update(ms, src_ino, addr);
    // Commit point (deferred, crash-consistent) — see namespace_changed.
    namespace_changed(ms);
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
    let ino = match resolve_path_r(ms, rel, follow) { Ok(i) => i, Err(e) => return err_reply(e) };
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
        inode_set_ctime(iblk, sched::clock_ts());
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
                cred: &xattr::Cred) -> Message {
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
    let ino = match resolve_path_r(ms, rel, follow) { Ok(i) => i, Err(e) => return err_reply(e) };
    chown_inode(ms, ino, uid, gid, cred)
}

/// fchown(2) — fd already resolved to an inode via the open-file table.
fn handle_fchown(ms: &mut MountState, file_id: u64, uid: u32, gid: u32,
                 cred: &xattr::Cred) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); }
    let ino = ms.open_files[slot].inode;
    chown_inode(ms, ino, uid, gid, cred)
}

/// Mutate i_uid/i_gid in place and write the inode block back, same
/// nat_update + maybe_flush shape as chmod_inode/handle_link.
fn chown_inode(ms: &mut MountState, ino: u32, uid: u32, gid: u32,
               cred: &xattr::Cred) -> Message {
    let addr = nat_lookup(ms, ino);
    {
        let iblk = ms.cache.read(ms.dev, addr as u64);
        let owner = inode_uid(iblk);
        if cred.euid != 0 {
            // Non-root: must own the file, may never hand it to someone else,
            // and may only set a group it belongs to (effective gid or any
            // supplementary group).
            if cred.euid != owner { return err_reply(-1); }          // EPERM
            if uid != u32::MAX && uid != owner { return err_reply(-1); }
            if gid != u32::MAX && !cred.in_group(gid) { return err_reply(-1); }
        }
    }
    let iblk = ms.cache.get_mut(ms.dev, addr as u64);
    if uid != u32::MAX { w32(iblk, INO_UID, uid); }
    if gid != u32::MAX { w32(iblk, INO_GID, gid); }
    inode_set_ctime(iblk, sched::clock_ts());
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
        if ms.cache.poisoned() { break; }
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
    let parent_ino = match resolve_parent(ms, parent_rel) {
        Ok(p) => p,
        Err(e) => return err_reply(e),
    };
    if !may_access_ino(ms, parent_ino, xattr::MAY_EXEC) { return err_reply(-13); }
    let ino = dir_lookup(ms, parent_ino, name);
    if ino == 0 { return err_reply(-2); }
    let iblkaddr = nat_lookup(ms, ino);
    let is_dir = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_is_dir(iblk) };
    if !is_dir { return err_reply(-20); } // ENOTDIR
    if let Err(e) = may_delete_in(ms, parent_ino, ino) { return err_reply(e); }
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
    // Commit point (deferred, crash-consistent) — see namespace_changed.
    namespace_changed(ms);
    ok_reply()
}

fn handle_rename(ms: &mut MountState, old_ptr: u64, new_ptr: u64, flags: u64) -> Message {
    const RENAME_NOREPLACE: u64 = 1;
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
    let old_parent_ino = match resolve_parent(ms, old_parent_rel) {
        Ok(p) => p, Err(e) => return err_reply(e),
    };
    let new_parent_ino = match resolve_parent(ms, new_parent_rel) {
        Ok(p) => p, Err(e) => return err_reply(e),
    };
    if !may_access_ino(ms, old_parent_ino, xattr::MAY_EXEC) { return err_reply(-13); }
    let ino = dir_lookup(ms, old_parent_ino, old_name);
    if ino == 0 { return err_reply(-2); }
    let iblkaddr = nat_lookup(ms, ino);
    let is_dir = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_is_dir(iblk) };

    // Permissions, Linux may_delete/may_create order: removing the source
    // name from its parent (write+search, sticky rule on the source), creating
    // the destination name (write+search on the new parent, sticky rule on a
    // replaced victim), and — for a directory changing parents — write on the
    // directory itself, since its ".." is rewritten.
    if let Err(e) = may_delete_in(ms, old_parent_ino, ino) { return err_reply(e); }
    if let Err(e) = may_modify_dir(ms, new_parent_ino) { return err_reply(e); }
    if is_dir && new_parent_ino != old_parent_ino
        && !may_access_ino(ms, ino, xattr::MAY_WRITE) {
        return err_reply(-13);
    }

    // POSIX rename atomically replaces an existing destination (the atomic-
    // write idiom — write tempfile, rename over the real name — depends on
    // it); only RENAME_NOREPLACE keeps refuse-with-EEXIST. Type rules first,
    // then the victim is dropped exactly the way unlink/rmdir would drop it.
    let dst_ino = dir_lookup(ms, new_parent_ino, new_name);
    if dst_ino != 0 {
        if flags & RENAME_NOREPLACE != 0 { return err_reply(-17); } // EEXIST
        if dst_ino == ino { return ok_reply(); } // same file — POSIX no-op
        if let Err(e) = may_delete_in(ms, new_parent_ino, dst_ino) { return err_reply(e); }
        let dst_iblkaddr = nat_lookup(ms, dst_ino);
        let dst_is_dir =
            { let iblk = ms.cache.read(ms.dev, dst_iblkaddr as u64); inode_is_dir(iblk) };
        if is_dir && !dst_is_dir { return err_reply(-20); } // ENOTDIR
        if !is_dir && dst_is_dir { return err_reply(-21); } // EISDIR
        if dst_is_dir && !dir_is_empty(ms, dst_ino) { return err_reply(-39); } // ENOTEMPTY
        if !dir_remove_entry(ms, new_parent_ino, new_name) { return err_reply(-2); }
        if dst_is_dir {
            // Same shape as rmdir: an empty dir's name is its last reference.
            if !ino_is_open(ms, dst_ino) {
                free_inode_data_and_nodes(ms, dst_ino);
                free_block(ms, dst_iblkaddr);
            }
        } else {
            // Same shape as unlink: honor surviving hard links and open fds.
            let links =
                { let iblk = ms.cache.read(ms.dev, dst_iblkaddr as u64); inode_links(iblk) };
            if links > 1 {
                let iblk = ms.cache.get_mut(ms.dev, dst_iblkaddr as u64);
                w32(iblk, INO_LINKS, links - 1);
                nat_update(ms, dst_ino, dst_iblkaddr);
            } else if ino_is_open(ms, dst_ino) {
                let iblk = ms.cache.get_mut(ms.dev, dst_iblkaddr as u64);
                w32(iblk, INO_LINKS, 0);
                nat_update(ms, dst_ino, dst_iblkaddr);
                mm::pagecache::set_orphan(page_cache_key(ms.port, dst_ino));
            } else {
                free_inode_data_and_nodes(ms, dst_ino);
                free_block(ms, dst_iblkaddr);
            }
        }
    }
    // The new dentry keeps the inode's file type. It used to say DT_REG for
    // everything but directories, so a renamed SYMLINK stopped being followed
    // (path walks trust the dentry type): exec through it read the link body
    // as an ELF and failed ENOEXEC. apk installs every symlink that way
    // (temp name + rename), e.g. busybox-binsh's /bin/sh -> /bin/busybox.
    let ftype = if is_dir { DT_DIR } else {
        let addr = nat_lookup(ms, ino);
        let m = { let iblk = ms.cache.read(ms.dev, addr as u64); inode_mode(iblk) };
        if m & S_IFMT == S_IFLNK { DT_LNK } else { DT_REG }
    };
    if !dir_add_entry(ms, new_parent_ino, new_name, ino, ftype) { return err_reply(-28); } // ENOSPC
    dir_remove_entry(ms, old_parent_ino, old_name);
    if is_dir && new_parent_ino != old_parent_ino {
        dir_remove_entry(ms, ino, b"..");
        dir_add_entry(ms, ino, b"..", new_parent_ino, DT_DIR);
    }
    // Commit point (deferred, crash-consistent) — see namespace_changed. Rename touches two
    // parent dentry blocks (+ the moved dir's ".."), the exact multi-block tear
    // the ?-type gap comes from.
    namespace_changed(ms);
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
        let iblkaddr = nat_lookup(ms, ino);
        let iblk = ms.cache.get_mut(ms.dev, iblkaddr as u64);
        inode_touch_mtime(iblk);
        nat_update(ms, ino, iblkaddr);
    } else {
        let iblk = ms.cache.get_mut(ms.dev, iblkaddr as u64);
        w64(iblk, INO_SIZE, length);
        inode_touch_mtime(iblk);
        nat_update(ms, ino, iblkaddr);
        // Growing: the old last page's bytes past the old end must read
        // zeros (a shared mapping may have stored there), then the size.
        if !ms.cache.poisoned() {
            let key = page_cache_key(ms.port, ino);
            mm::pagecache::extend(key, old_size, length);
            mm::pagecache::note_size(key, length);
        }
    }
    maybe_flush(ms);
    ok_reply()
}

/// `VFS_FALLOCATE(file_id, mode, offset, len)` — the VFS has already
/// validated the range and rejected every mode but 0 and `FALLOC_FL_KEEP_SIZE`
/// (punching holes is EOPNOTSUPP on this volume).
///
/// Mode 0 grows `i_size` to `offset + len` exactly as a growing ftruncate does;
/// KEEP_SIZE changes nothing. Neither reserves blocks — this F2FS allocates at
/// write time only, so a later write can still meet ENOSPC — but the file-size
/// contract posix_fallocate(3) callers rely on (musl does not emulate it) holds.
fn handle_fallocate(ms: &mut MountState, file_id: u64, mode: u64, offset: u64, len: u64) -> Message {
    let slot = file_id as usize;
    if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return err_reply(-9); }
    if !ms.open_files[slot].writable { return err_reply(-9); } // EBADF, as Linux
    if mode != 0 { return ok_reply(); } // KEEP_SIZE: size unchanged, nothing reserved
    let end = offset.saturating_add(len);
    let ino = ms.open_files[slot].inode;
    let iblkaddr = nat_lookup(ms, ino);
    let old_size = { let iblk = ms.cache.read(ms.dev, iblkaddr as u64); inode_size(iblk) };
    if end <= old_size { return ok_reply(); }
    handle_ftruncate(ms, file_id, end)
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
/// `f2fs.statfs_check=1` (kernel command line): every statfs also sums the
/// SIT and reports a counter mismatch (the first agreement is logged once).
fn statfs_check() -> bool {
    static KNOB: atomic::AtomicU8 = atomic::AtomicU8::new(0);
    match KNOB.load(atomic::Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = drivers::nvidia_probe::cmdline_opt("f2fs.statfs_check").unwrap_or(0) != 0;
            KNOB.store(if on { 2 } else { 1 }, atomic::Ordering::Relaxed);
            on
        }
    }
}
static STATFS_CHECK_OK: atomic::AtomicBool = atomic::AtomicBool::new(false);

fn handle_statfs(ms: &mut MountState, buf_ptr: u64) -> Message {
    if buf_ptr == 0 { return err_reply(-14); } // EFAULT

    let bps          = ms.sb.blocks_per_seg as u64;
    let user_blocks  = ms.sb.seg_cnt_main as u64 * bps;
    let total_blocks = ms.sb.block_count.saturating_sub(ms.sb.segment0_blkaddr as u64);
    // A superblock we failed to make sense of must still not report zero —
    // `df` silently drops any filesystem with f_blocks == 0.
    let total_blocks = if total_blocks == 0 { user_blocks.max(1) } else { total_blocks };

    let valid_blocks = ms.valid_blocks;
    if statfs_check() {
        let scan = sit_count_valid_blocks(ms);
        if ms.cache.poisoned() {
            // A SIT block was not cached: this request runs again once it is.
        } else if scan != valid_blocks {
            dbg_str("[F2FS] statfs cross-check MISMATCH: counter ");
            dbg_dec(valid_blocks as usize);
            dbg_str(" full SIT scan ");
            dbg_dec(scan as usize);
            dbg_str("\n");
        } else if !STATFS_CHECK_OK.swap(true, atomic::Ordering::Relaxed) {
            dbg_str("[F2FS] statfs cross-check ok: ");
            dbg_dec(scan as usize);
            dbg_str(" valid blocks\n");
        }
    }
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
            inode_add_blocks(iblk, 1);
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
    match resolve_path_r(ms, rel, follow) { Ok(i) => Ok(i), Err(e) => Err(err_reply(e)) }
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
             cred: &xattr::Cred) -> Message {
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
        if let Err(e) = xattr::may_read_xattr(idx, &meta, cred, None) { return err_reply(-e); }
        return err_reply(-61); // ENODATA
    }
    let blkbuf = read_xattr_arena(ms, xnid);
    let arena = &blkbuf[..xattr::F2FS_XATTR_ARENA];
    let acl = xattr::find(arena, xattr::IDX_ACL_ACCESS, b"");
    if let Err(e) = xattr::may_read_xattr(idx, &meta, cred, acl) { return err_reply(-e); }
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
             flags: u64, cred: &xattr::Cred) -> Message {
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
        if let Err(e) = xattr::may_write_xattr(idx, &meta, cred, acl) { return err_reply(-e); }
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

fn xattr_remove(ms: &mut MountState, ino: u32, name_ptr: u64, cred: &xattr::Cred) -> Message {
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
        if let Err(e) = xattr::may_write_xattr(idx, &meta, cred, acl) { return err_reply(-e); }
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
fn xattr_access(ms: &mut MountState, path_ptr: u64, amode: u64, cred: &xattr::Cred) -> Message {
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
    if xattr::access_check(&meta, cred, acl, amode & 4 != 0, amode & 2 != 0, amode & 1 != 0) {
        ok_reply()
    } else {
        err_reply(-13) // EACCES
    }
}

// ── creation: default-ACL inheritance ────────────────────────────────────────

/// The parent's default ACL, copied out of its xattr node (None when absent).
fn parent_default_acl(ms: &mut MountState, parent_ino: u32, out: &mut [u8; xattr::ACL_INHERIT_MAX]) -> usize {
    let (_, xnid) = load_meta_xnid(ms, parent_ino);
    if xnid == 0 { return 0; }
    let blk = read_xattr_arena(ms, xnid);
    match xattr::find(&blk[..xattr::F2FS_XATTR_ARENA], xattr::IDX_ACL_DEFAULT, b"") {
        Some(v) if v.len() <= out.len() => { out[..v.len()].copy_from_slice(v); v.len() }
        _ => 0,
    }
}

/// `xattr::acl_create` against the parent's default ACL: the child's mode
/// (umask applied only when there is no default ACL) plus what to store.
fn inherit_create_mode(ms: &mut MountState, parent_ino: u32, mode: u16, umask: u16, is_dir: bool) -> xattr::Created {
    let mut dbuf = [0u8; xattr::ACL_INHERIT_MAX];
    let dlen = parent_default_acl(ms, parent_ino, &mut dbuf);
    let dacl = if dlen > 0 { Some(&dbuf[..dlen]) } else { None };
    xattr::acl_create(dacl, mode, umask, is_dir)
}

/// Give a freshly created inode its inherited access ACL and (directories)
/// the parent's default ACL, in a new xattr node block. No-op when there is
/// nothing to store — the common case costs no block.
fn store_inherited_acls(ms: &mut MountState, ino: u32, parent_ino: u32, c: &xattr::Created) {
    if c.access_len == 0 && !c.inherit_default { return; }
    let mut dbuf = [0u8; xattr::ACL_INHERIT_MAX];
    let dlen = if c.inherit_default { parent_default_acl(ms, parent_ino, &mut dbuf) } else { 0 };
    if c.access_len == 0 && dlen == 0 { return; }
    let mut blkbuf = [0u8; BLOCK_SIZE];
    {
        let arena = &mut blkbuf[..xattr::F2FS_XATTR_ARENA];
        if c.access_len > 0 {
            let _ = xattr::set(arena, xattr::IDX_ACL_ACCESS, b"", &c.access[..c.access_len], 0);
        }
        if dlen > 0 {
            let _ = xattr::set(arena, xattr::IDX_ACL_DEFAULT, b"", &dbuf[..dlen], 0);
        }
    }
    persist_xattr_block(ms, ino, 0, &blkbuf);
}

// ── utimensat ────────────────────────────────────────────────────────────────

/// Apply decoded times to inode `ino` after the utimensat(2) permission rule
/// (see the VFS's `utimens_permission`): explicit times need ownership or
/// root (EPERM); "now" needs ownership, root, or write permission (EACCES).
fn utimens_inode(ms: &mut MountState, ino: u32, at: (i64, i64), mt: (i64, i64), explicit: bool,
                 cred: &xattr::Cred) -> Message {
    let (meta, xnid) = load_meta_xnid(ms, ino);
    if cred.euid != 0 && cred.euid != meta.uid {
        if explicit { return err_reply(-1); } // EPERM
        let xbuf = if xnid != 0 { Some(read_xattr_arena(ms, xnid)) } else { None };
        let acl = match &xbuf {
            Some(b) => xattr::find(&b[..xattr::F2FS_XATTR_ARENA], xattr::IDX_ACL_ACCESS, b""),
            None => None,
        };
        if !xattr::may_access(&meta, cred, acl, xattr::MAY_WRITE) { return err_reply(-13); } // EACCES
    }
    let addr = nat_lookup(ms, ino);
    if addr == 0 { return err_reply(-2); }
    {
        let iblk = ms.cache.get_mut(ms.dev, addr as u64);
        if at.1 != vfs_server::UTIME_OMIT { inode_set_atime(iblk, at); }
        if mt.1 != vfs_server::UTIME_OMIT { inode_set_mtime(iblk, mt); }
        inode_set_ctime(iblk, sched::clock_ts());
    }
    mm::pagecache::note_ctime(page_cache_key(ms.port, ino));
    nat_update(ms, ino, addr);
    maybe_flush(ms);
    ok_reply()
}

fn handle_utimens(ms: &mut MountState, msg: &Message, follow: bool, cred: &xattr::Cred) -> Message {
    let ino = match xattr_path_ino(ms, arg(msg, 0), follow) { Ok(i) => i, Err(m) => return m };
    utimens_inode(ms, ino, (arg(msg, 1) as i64, arg(msg, 2) as i64),
                  (arg(msg, 3) as i64, arg(msg, 4) as i64),
                  arg(msg, 5) & vfs_server::UTIMENS_EXPLICIT != 0, cred)
}

fn handle_futimens(ms: &mut MountState, msg: &Message, cred: &xattr::Cred) -> Message {
    let ino = match xattr_fd_ino(ms, arg(msg, 0)) { Ok(i) => i, Err(m) => return m };
    utimens_inode(ms, ino, (arg(msg, 1) as i64, arg(msg, 2) as i64),
                  (arg(msg, 3) as i64, arg(msg, 4) as i64),
                  arg(msg, 5) & vfs_server::UTIMENS_EXPLICIT != 0, cred)
}

// ── IPC dispatch ──────────────────────────────────────────────────────────────

fn f2fs_dispatch(msg: &Message, caller_pid: u32, target_port: u32) -> Message {
    let port = target_port;
    match msg.tag {
        VFS_READ => {
            // read(2): the slot's offset under its lock, the data through the
            // page cache (a hit needs no filesystem lock at all).
            let slot = arg(msg, 0) as usize;
            let g = match SlotGuard::take(port, slot) { Some(g) => g, None => return err_reply(-9) };
            let ino = g.ino();
            if ino == 0 { return err_reply(-9); } // EBADF
            let pos = g.pos();
            let r = pc_read(port, ino, pos, arg(msg, 1) as *mut u8, arg(msg, 2) as usize, PcMode::Read);
            if r > 0 {
                g.set_pos(pos + r as u64);
                drop(g);
                pc_atime(port, ino);
            }
            return if r < 0 { err_reply(r as i32) } else { val_reply(r as u64) };
        }
        VFS_WRITE => {
            // `VFS_WRITE(file_id, buf_ptr, count, append)`. `append` is the
            // VFS's O_APPEND bit for the *descriptor* that issued this write:
            // the write goes to the current end of file (Linux re-derives EOF
            // on every write, not once at open) and leaves the offset just
            // past what it wrote. An intervening lseek is irrelevant, exactly
            // as on Linux; pwrite on an O_APPEND descriptor appends too.
            let slot = arg(msg, 0) as usize;
            let g = match SlotGuard::take(port, slot) { Some(g) => g, None => return err_reply(-9) };
            let at = if arg(msg, 3) != 0 { WritePos::Append } else { WritePos::At(g.pos()) };
            let (r, end) = write_at(port, slot, arg(msg, 1), arg(msg, 2) as usize, at, caller_pid);
            if r > 0 { g.set_pos(end); }
            return if r < 0 { err_reply(r as i32) } else { val_reply(r as u64) };
        }
        VFS_LSEEK => {
            let slot = arg(msg, 0) as usize;
            let g = match SlotGuard::take(port, slot) { Some(g) => g, None => return err_reply(-9) };
            let (offset, whence) = (arg(msg, 1), arg(msg, 2));
            let new_pos = match whence {
                0 => offset,
                1 => g.pos().wrapping_add(offset),
                2 => {
                    let size = with_mount(port, |ms| {
                        act_code(VFS_LSEEK);
                        if slot >= MAX_OPEN_FILES || !ms.open_files[slot].in_use { return None; }
                        let iblkaddr = nat_lookup(ms, ms.open_files[slot].inode);
                        Some(inode_size(ms.cache.read(ms.dev, iblkaddr as u64)))
                    });
                    match size {
                        Some(Some(sz)) => sz.wrapping_add(offset),
                        Some(None) => return err_reply(-9),
                        None => return err_reply(-5),
                    }
                }
                _ => return err_reply(-22),
            };
            if g.ino() == 0 { return err_reply(-9); }
            g.set_pos(new_pos);
            return val_reply(new_pos);
        }
        VFS_GETDENTS64 => {
            let slot = arg(msg, 0) as usize;
            let g = match SlotGuard::take(port, slot) { Some(g) => g, None => return err_reply(-9) };
            let pos = g.pos();
            let r = with_mount(port, |ms| {
                act_code(VFS_GETDENTS64);
                ms.cred = vfs_server::cred_of(caller_pid);
                handle_getdents(ms, arg(msg, 0), arg(msg, 1), arg(msg, 2), pos)
            });
            return match r {
                Some((reply, new_pos)) => { g.set_pos(new_pos); reply }
                None => err_reply(-5),
            };
        }
        VFS_FSYNC => {
            // fsync(2) and sync(2): dirty pages of shared mappings first, then
            // a checkpoint whose device writes run without the filesystem lock
            // (`checkpoint_unlocked`, lane loginhang), shared by concurrent
            // callers (group commit, lane f2fsperf).
            // The page cache's dirty pages must reach this server as normal
            // writes BEFORE fsync_port decides from its per-file records
            // whether a checkpoint is needed (lane fsyncbatch): a file whose
            // only change is still in the page cache looks clean to it.
            // data[0] = file id, data[1] = kind (FSYNC_KIND_*; 0 = sync(2)).
            let (fid, kind) = (arg(msg, 0), arg(msg, 1));
            let ino = if kind == FSYNC_KIND_FILE || kind == FSYNC_KIND_DATA {
                slot_fast(port, fid as usize).map_or(0, |s| s.ino.load(atomic::Ordering::Acquire))
            } else { 0 };
            if ino != 0 {
                let key = page_cache_key(port, ino);
                pc_flush_dirty_keys(key, key);
            } else {
                pc_flush_dirty(Some(port), true);
            }
            return fsync_port(port, caller_pid, fid, kind);
        }
        _ => {}
    }
    dispatch_locked(msg, caller_pid, target_port).unwrap_or_else(|| err_reply(-5)) // EIO — no mount found for this port
}

/// One request under the lock, then (lock released) the write-back policy:
/// a request that left `hard_pinned` slots dirty or in write-back runs a
/// checkpoint itself before it returns (writer throttle: the cache must not
/// fill with blocks it may not evict); one that left `soft_dirty` dirty
/// slots only wakes the write-back task.
fn dispatch_locked(msg: &Message, caller_pid: u32, port: u32) -> Option<Message> {
    // `with_mount`: the request runs with the lock held but never waits for
    // the device under it — a block it needs that is not cached is read with
    // the lock dropped and the request is run again (lane nostall).
    let (reply, throttle, kick) = match with_mount(port, |ms| {
        // A request that names its file by descriptor: its changes are that
        // inode's, for per-file fsync (lane fsyncbatch).
        ms.req_ino = fd_request_ino(ms, msg);
        let r = dispatch_msg(ms, msg, caller_pid);
        let c = &ms.cache;
        // Only requests that can dirty the cache are throttled: a
        // reader must never wait out somebody else's write-back.
        let rw = !ms.read_only && (is_mutating(msg.tag) || msg.tag == VFS_OPEN);
        (r, rw && c.npinned >= c.hard_pinned(), rw && c.ndirty >= c.soft_dirty())
    }) {
        Some((r, t, k)) => (Some(r), t, k),
        None => (None, false, false),
    };
    if throttle {
        THROTTLE_COUNT.fetch_add(1, atomic::Ordering::Relaxed);
        let _ = checkpoint_unlocked(port, None, CpWhy::Throttle);
    } else if kick {
        kick_writeback(port);
    }
    reply
}

/// The inode a request changes when it names the file by descriptor
/// (arg 0 is the mount-local file id for every one of these), else 0.
fn fd_request_ino(ms: &MountState, msg: &Message) -> u32 {
    match msg.tag {
        VFS_WRITE | VFS_FTRUNCATE | VFS_FALLOCATE | VFS_FCHMOD | VFS_FCHOWN | VFS_FSETXATTR
        | VFS_FREMOVEXATTR | VFS_FUTIMENS => {
            let slot = arg(msg, 0) as usize;
            if slot < MAX_OPEN_FILES && ms.open_files[slot].in_use { ms.open_files[slot].inode } else { 0 }
        }
        _ => 0,
    }
}

/// A write(2) larger than this is applied in pieces, each its own locked
/// request followed by the write-back policy, so one huge write can neither
/// hold the filesystem for its whole duration nor dirty more of the cache
/// than the writer throttle allows (`write_at`).
const WRITE_CHUNK: usize = 1 << 20;

fn dispatch_msg(ms: &mut MountState, msg: &Message, caller_pid: u32) -> Message {
    act_code(msg.tag);
    // Published for the path walk (`resolve_path_ex`) and the parent-directory
    // gates (`may_access_ino`), which run below this frame for every path op.
    //
    // Note `vfs_server::cred_of` answers root for a pid it cannot find, i.e.
    // it fails *open* to root. That is the right answer for the boot-time
    // mount path, which runs before there is a user process to attribute, but
    // it is a deliberate choice rather than an accident.
    ms.cred = vfs_server::cred_of(caller_pid);
    let cred = ms.cred;
    if ms.read_only && is_mutating(msg.tag) {
        return err_reply(-30); // EROFS
    }
    match msg.tag {
        VFS_OPEN       => handle_open(ms, arg(msg,0), arg(msg,1), arg(msg,2), &cred),
        VFS_CLOSE      => handle_close(ms, arg(msg,0)),
        VFS_STAT       => handle_stat(ms, arg(msg,0), arg(msg,1)),
        VFS_FSTAT      => handle_fstat(ms, arg(msg,0), arg(msg,1)),
        VFS_MKDIR      => handle_mkdir(ms, arg(msg,0), arg(msg,1), &cred),
        VFS_UNLINK     => handle_unlink(ms, arg(msg,0)),
        VFS_RMDIR      => handle_rmdir(ms, arg(msg,0)),
        VFS_RENAME     => handle_rename(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_FTRUNCATE  => handle_ftruncate(ms, arg(msg,0), arg(msg,1)),
        VFS_FALLOCATE  => handle_fallocate(ms, arg(msg,0), arg(msg,1), arg(msg,2), arg(msg,3)),
        VFS_STATFS     => handle_statfs(ms, arg(msg,1)),
        VFS_LSTAT      => handle_lstat(ms, arg(msg,0), arg(msg,1)),
        VFS_SYMLINK    => handle_symlink(ms, arg(msg,0), arg(msg,1), &cred),
        VFS_FD_PATH    => handle_fd_path(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_READLINK   => handle_readlink(ms, arg(msg,0), arg(msg,1), arg(msg,2)),
        VFS_LINK       => handle_link(ms, arg(msg,0), arg(msg,1)),
        VFS_CHMOD      => handle_chmod(ms, arg(msg,0), arg(msg,1) as u32, true, cred.euid),
        VFS_LCHMOD     => handle_chmod(ms, arg(msg,0), arg(msg,1) as u32, false, cred.euid),
        VFS_FCHMOD     => handle_fchmod(ms, arg(msg,0), arg(msg,1) as u32, cred.euid),
        VFS_FSYNC      => handle_fsync(ms),
        VFS_CHOWN      => handle_chown(ms, arg(msg,0), arg(msg,1) as u32, arg(msg,2) as u32, true, &cred),
        VFS_LCHOWN     => handle_chown(ms, arg(msg,0), arg(msg,1) as u32, arg(msg,2) as u32, false, &cred),
        VFS_FCHOWN     => handle_fchown(ms, arg(msg,0), arg(msg,1) as u32, arg(msg,2) as u32, &cred),

        // Extended attributes. Path forms carry (path, name, value, size, flags);
        // f-forms replace path with a mount-local file_id. The l-forms differ
        // only in not following a final symlink. Args past the third are read
        // from the same inline payload (Message.data holds 55 u64 words).
        VFS_SETXATTR | VFS_LSETXATTR =>
            match xattr_path_ino(ms, arg(msg,0), msg.tag == VFS_SETXATTR) {
                Ok(ino) => xattr_set(ms, ino, arg(msg,1), arg(msg,2), arg(msg,3), arg(msg,4), &cred),
                Err(m) => m,
            },
        VFS_FSETXATTR =>
            match xattr_fd_ino(ms, arg(msg,0)) {
                Ok(ino) => xattr_set(ms, ino, arg(msg,1), arg(msg,2), arg(msg,3), arg(msg,4), &cred),
                Err(m) => m,
            },
        VFS_GETXATTR | VFS_LGETXATTR =>
            match xattr_path_ino(ms, arg(msg,0), msg.tag == VFS_GETXATTR) {
                Ok(ino) => xattr_get(ms, ino, arg(msg,1), arg(msg,2), arg(msg,3), &cred),
                Err(m) => m,
            },
        VFS_FGETXATTR =>
            match xattr_fd_ino(ms, arg(msg,0)) {
                Ok(ino) => xattr_get(ms, ino, arg(msg,1), arg(msg,2), arg(msg,3), &cred),
                Err(m) => m,
            },
        VFS_LISTXATTR | VFS_LLISTXATTR =>
            match xattr_path_ino(ms, arg(msg,0), msg.tag == VFS_LISTXATTR) {
                Ok(ino) => xattr_list(ms, ino, arg(msg,1), arg(msg,2), cred.euid),
                Err(m) => m,
            },
        VFS_FLISTXATTR =>
            match xattr_fd_ino(ms, arg(msg,0)) {
                Ok(ino) => xattr_list(ms, ino, arg(msg,1), arg(msg,2), cred.euid),
                Err(m) => m,
            },
        VFS_REMOVEXATTR | VFS_LREMOVEXATTR =>
            match xattr_path_ino(ms, arg(msg,0), msg.tag == VFS_REMOVEXATTR) {
                Ok(ino) => xattr_remove(ms, ino, arg(msg,1), &cred),
                Err(m) => m,
            },
        VFS_FREMOVEXATTR =>
            match xattr_fd_ino(ms, arg(msg,0)) {
                Ok(ino) => xattr_remove(ms, ino, arg(msg,1), &cred),
                Err(m) => m,
            },
        VFS_ACCESS     => {
            // arg(msg,2) is AT_EACCESS (0/1 from the kernel's faccessat). The
            // POSIX default for access()/faccessat() is the REAL ids, not the
            // effective ones `ms.cred` above was just built with.
            let acred = if arg(msg,2) != 0 { cred } else { vfs_server::real_cred_of(caller_pid) };
            xattr_access(ms, arg(msg,0), arg(msg,1), &acred)
        }
        VFS_UTIMENS    => handle_utimens(ms, msg, true, &cred),
        VFS_LUTIMENS   => handle_utimens(ms, msg, false, &cred),
        VFS_FUTIMENS   => handle_futimens(ms, msg, &cred),

        _              => err_reply(-22), // EINVAL
    }
}

/// Requests that change the volume, refused with EROFS on a read-only mount.
/// (VFS_OPEN is gated on its flags in `handle_open`.)
fn is_mutating(tag: u64) -> bool {
    matches!(tag,
        VFS_WRITE | VFS_MKDIR | VFS_UNLINK | VFS_RMDIR | VFS_RENAME | VFS_FTRUNCATE
        | VFS_FALLOCATE | VFS_SYMLINK | VFS_LINK | VFS_CHMOD | VFS_LCHMOD | VFS_FCHMOD
        | VFS_CHOWN | VFS_LCHOWN | VFS_FCHOWN | VFS_SETXATTR | VFS_LSETXATTR
        | VFS_FSETXATTR | VFS_REMOVEXATTR | VFS_LREMOVEXATTR | VFS_FREMOVEXATTR
        | VFS_UTIMENS | VFS_LUTIMENS | VFS_FUTIMENS)
}

// ── Public API ────────────────────────────────────────────────────────────────

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
    // Linux's f2fs tells a clean volume from one that needs roll-forward
    // recovery by CP_UMOUNT_FLAG on the newest checkpoint; say which this is,
    // once per mount, so a shutdown that skipped the final sync is visible.
    dbg_str("[F2FS] ");
    dbg_str(virtio_blk::dev_path(dev_idx));
    dbg_str(": checkpoint ver ");
    dbg_dec(cp.ver as usize);
    if cp.flags & CP_UMOUNT_FLAG != 0 {
        dbg_str(" is a clean unmount\n");
    } else {
        dbg_str(" is NOT a clean unmount: the previous shutdown was unclean (writes after it were lost)\n");
    }

    // Allocate a slot in the global mount table
    let mut mounts = lock_mounts();
    let slot_idx = mounts.iter().position(|s| s.is_none())?;

    let port = port::create(owner_pid)?;
    port::register_handler(port, f2fs_dispatch);

    // Build the mount state directly in its heap allocation, field by field.
    //
    // `Box::new(MountState { .. })` is the obvious spelling and is a trap here:
    // rustc materialises the ~72 KB struct as a stack temporary and memcpys it
    // in, which measured at `sub rsp, 0x260E8` — a 155,880 B frame. `mount` runs
    // in a syscall on the caller's kernel stack (`sched::KERNEL_STACK_SIZE`),
    // which is 128 KiB and was 64 KiB when this was written, so that frame
    // overran the stack outright. It is the exact defect of TODO.md item 15,
    // reintroduced by the obvious spelling of the fix for it. The two big
    // fields are filled element-wise for the same reason: `BlockCache::new()`
    // alone is a ~16 KB temporary and `[OpenFile::empty(); MAX_OPEN_FILES]`
    // is ~55 KB.
    let mut uninit: Box<core::mem::MaybeUninit<MountState>> = Box::new_uninit();
    let state: Box<MountState> = unsafe {
        let p = uninit.as_mut_ptr();
        core::ptr::addr_of_mut!((*p).dev).write(dev_idx);
        core::ptr::addr_of_mut!((*p).port).write(port);
        core::ptr::addr_of_mut!((*p).mount_prefix).write(mount_point);
        core::ptr::addr_of_mut!((*p).sb).write(sb);
        core::ptr::addr_of_mut!((*p).cp).write(cp);
        core::ptr::addr_of_mut!((*p).dirty_writes).write(0);
        core::ptr::addr_of_mut!((*p).last_cp_ns).write(sched::monotonic_ns());
        core::ptr::addr_of_mut!((*p).first_dirty_ns).write(0);
        core::ptr::addr_of_mut!((*p).last_cp_pos).write([u32::MAX; 6]);
        core::ptr::addr_of_mut!((*p).valid_blocks).write(0);
        core::ptr::addr_of_mut!((*p).cred).write(xattr::Cred::ROOT);
        core::ptr::addr_of_mut!((*p).walk_err).write(0);
        core::ptr::addr_of_mut!((*p).read_only).write(false);
        core::ptr::addr_of_mut!((*p).of_undo).write([(0, OpenFile::empty()); OF_UNDO_MAX]);
        core::ptr::addr_of_mut!((*p).of_undo_n).write(0);
        core::ptr::addr_of_mut!((*p).ino_mod).write(alloc::vec![0u64; INO_MOD_SLOTS]);
        core::ptr::addr_of_mut!((*p).mod_global).write(0);
        core::ptr::addr_of_mut!((*p).req_ino).write(0);
        core::ptr::addr_of_mut!((*p).req_marked).write(false);
        let files = core::ptr::addr_of_mut!((*p).open_files) as *mut OpenFile;
        for i in 0..MAX_OPEN_FILES {
            files.add(i).write(OpenFile::empty());
        }
        core::ptr::addr_of_mut!((*p).cache).write(BlockCache::new(cache_cap()));
        (*p).cache.dev_blocks = sb.block_count;
        uninit.assume_init()
    };
    mounts[slot_idx] = Some(state);
    if let Some(ms) = mounts[slot_idx].as_deref_mut() {
        ms.valid_blocks = sit_count_valid_blocks(ms);
    }
    let s = &CP_SEQ[slot_idx];
    s.done.store(s.started.load(atomic::Ordering::SeqCst), atomic::Ordering::SeqCst);
    s.port.store(port, atomic::Ordering::Release);

    drop(mounts);
    fsync_cfg_load();
    pc_configure();
    start_wb_thread();
    start_pcache_thread();
    // The block registry's `/dev/<name>` (vdb, sda2, …), for /proc/mounts.
    let device_str = virtio_blk::dev_path(dev_idx);
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
    // Through the page cache (exec's per-open fallback backing).
    if let Some(s) = slot_fast(port, file_id as usize) {
        let ino = s.ino.load(atomic::Ordering::Acquire);
        if ino != 0 { return pc_read(port, ino, pos, dst, len, PcMode::Fault); }
    }
    let resolve = |ms: &MountState| -> Result<(u32, u64), isize> {
        let idx = file_id as usize;
        if idx >= MAX_OPEN_FILES || !ms.open_files[idx].in_use {
            return Err(-9); // EBADF
        }
        Ok((ms.open_files[idx].inode, pos))
    };
    read_unlocked(port, dst, len, &resolve, None)
}

/// Inode behind open-file slot `file_id` on the mount at `port`.
pub fn inode_by_port(port: u32, file_id: u64) -> Option<u32> {
    let mounts = lock_mounts();
    for slot in mounts.iter() {
        if let Some(ref ms) = slot {
            if ms.port == port {
                let idx = file_id as usize;
                if idx >= MAX_OPEN_FILES || !ms.open_files[idx].in_use { return None; }
                return Some(ms.open_files[idx].inode);
            }
        }
    }
    None
}

/// Positional read by inode, for the kernel's private file mappings: the
/// mapping holds no descriptor (it outlives the one it was made from), only
/// a `pin_inode` pin. Same contract as `pread_by_port`; short at EOF.
pub fn pread_ino_by_port(port: u32, ino: u32, dst: *mut u8, len: usize, pos: u64) -> isize {
    pc_read(port, ino, pos, dst, len, PcMode::Fault)
}

/// Close an open-file slot on the mount whose IPC port is `port` — the
/// direct-call twin of VFS_CLOSE, used when the kernel releases the backing
/// file of a demand-paged exec image. May be called where sleeping is not
/// allowed (munmap under the address-space lock, an address space dropped
/// in the scheduler loop): then, if the mount table is busy, the close is
/// queued and applied by the next locker instead of spinning for the lock.
pub fn close_by_port(port: u32, file_id: u64) {
    let mut mounts = if sched::irqctx::can_sleep() {
        lock_mounts()
    } else {
        match F2FS_MOUNTS.try_lock() {
            Some(g) => { let mut g = MountsGuard::new(g); apply_pending_closes(&mut g); g }
            None => { PENDING_CLOSE.lock().push((port, file_id)); return; }
        }
    };
    for slot in mounts.iter_mut() {
        if let Some(ref mut ms) = slot {
            if ms.port == port {
                let idx = file_id as usize;
                if idx < MAX_OPEN_FILES {
                    ms.open_files[idx].in_use = false;
                    slot_closed(port, idx);
                }
                return;
            }
        }
    }
}

/// Unmount the F2FS volume mounted at `mount_point`.
/// Returns true on success, false if no volume was mounted at that point.
pub fn unmount(mount_point: &str) -> bool {
    let mut mounts = lock_mounts();
    if let Some(slot_idx) = mounts.iter().position(|s| s.as_ref().map_or(false, |m| m.mount_prefix == mount_point)) {
        if let Some(mut ms) = mounts[slot_idx].take() {
            // Flush cache and checkpoint, marked as a clean unmount.
            if !ms.read_only { write_checkpoint(&mut ms, true); }
            CP_SEQ[slot_idx].port.store(0, atomic::Ordering::Release);
            // Close IPC port (implicitly unregisters handler)
            port::close(ms.port);
            // Unregister from VFS
            vfs_server::unregister_mount(mount_point);
            return true;
        }
    }
    false
}


/// Switch the mount whose IPC port is `port` to read-only (`ro`) or back to
/// read-write — `mount -o remount,ro|rw`. Going read-only flushes everything
/// and commits a clean-unmount checkpoint first, exactly what Linux's
/// `f2fs_remount` does via `f2fs_sync_fs` + the umount checkpoint, so a volume
/// left read-only (the root fs at shutdown) is clean even if it is never
/// unmounted. Returns false when no f2fs volume is behind `port`.
pub fn remount_by_port(port: u32, ro: bool) -> bool {
    let mut mounts = lock_mounts();
    for slot in mounts.iter_mut() {
        if let Some(ref mut ms) = slot {
            if ms.port == port {
                if ro && !ms.read_only {
                    write_checkpoint(ms, true);
                    ms.read_only = true;
                } else if !ro {
                    ms.read_only = false;
                }
                return true;
            }
        }
    }
    false
}

/// The filesystem half of `kernel_power_off`/`kernel_restart`: commit every
/// mounted volume with a clean-unmount checkpoint and make it read-only, so
/// nothing that still runs can dirty it again before the machine stops. A
/// volume already read-only (init remounted it) is left as it is. Returns how
/// many volumes this call checkpointed.
pub fn shutdown_all() -> usize {
    // Pages stored through shared mappings reach the volume first.
    if sched::irqctx::can_sleep() && sched::current_pid() != 0 { pc_flush_dirty(None, true); }
    let mut mounts = lock_mounts();
    let mut n = 0;
    for slot in mounts.iter_mut() {
        if let Some(ref mut ms) = slot {
            if !ms.read_only {
                write_checkpoint(ms, true);
                ms.read_only = true;
                n += 1;
            }
        }
    }
    n
}
