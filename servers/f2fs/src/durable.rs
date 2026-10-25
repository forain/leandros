//! Crash consistency of this f2fs server (lane f2fsync, 2026-10-19):
//! atomic checkpoints, roll-forward fsync and node-id recycling.
//!
//! ## Why a journal
//!
//! This server updates node and data blocks in place, and NAT and SIT have a
//! single copy each (no upstream NAT/SIT sets with version bitmaps). A
//! checkpoint used to write every dirty block home and then the commit block,
//! so a crash in the middle left some NAT/SIT/inode/dentry blocks of the new
//! state next to the rest of the old one: the old commit block then described
//! a volume that no longer existed (blocks referenced but free in the SIT,
//! dentries naming inodes the NAT did not map).
//!
//! Now a checkpoint is a redo transaction. Every dirty block is classified:
//!
//! - **direct**: regular-file data (tagged by `write_file_data`), or a block
//!   allocated since the last snapshot ("fresh": free in the durable
//!   checkpoint, and the allocator never reuses a block freed since it, see
//!   `prot_*`). Nothing durable refers to its old contents, so it is written
//!   in place before the commit.
//! - **journal**: everything else (NAT, SIT, existing inode/dnode blocks,
//!   existing dentry blocks). Their images go to the free blocks of the
//!   checkpoint pack being written (an index of home addresses first), and
//!   only after the commit block is on the medium are they written home.
//!
//! Order: direct + journal, flush, commit block (CRC32, journal size and
//! CRC), flush, home writes. A crash before the commit leaves every home
//! location as the previous checkpoint wrote it; after it, the mount replays
//! the journal of the newest valid pack (idempotent). A journal larger than
//! the pack's 511 spare blocks continues in whole free main-area segments
//! (listed in the commit block, reserved until the next checkpoint commits).
//!
//! ## Roll-forward fsync
//!
//! fsync of a regular file no longer needs a checkpoint. With CP_MUTEX held
//! (no checkpoint in flight) it writes the file's dirty data blocks in place,
//! flushes, appends one record to the durable pack (after its journal): a
//! header listing `(nid, home address)` of every dirty node block of the file
//! plus their images, CRC-protected and stamped with the durable checkpoint's
//! version and a sequence number, then flushes. The node blocks stay dirty
//! and reach home through the next checkpoint, which makes the records
//! obsolete. Cost is proportional to the file's dirty state, not the volume's.
//!
//! The mount replays the records (upstream f2fs's roll-forward recovery):
//! per inode it walks the block tree as the checkpoint has it, installs the
//! images (NAT entries follow), walks it again, and moves the difference in
//! the SIT (blocks only the new tree uses become valid, blocks only the old
//! tree used become free, nids it dropped lose their NAT entry). A file
//! created since the checkpoint (and touched by no other namespace operation)
//! carries the dentry mark: its name is re-added to its parent from the
//! inode's i_pino/i_name. Any other namespace change of the file since the
//! checkpoint (link, unlink, rename) or a directory makes fsync take a full
//! checkpoint, as before.
//!
//! ## Node ids
//!
//! A freed node id loses its NAT entry and becomes reusable once a checkpoint
//! recording the free is on the medium (pending -> in flight -> pool). When
//! the bump allocator reaches the end of the NAT the NAT is scanned for free
//! entries.

use super::*;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

// ── On-disk format ───────────────────────────────────────────────────────────

/// Commit-block extension: "LPJ1" at `CPJ_MAGIC_OFF` marks a pack written by
/// this design (the commit block then carries a CRC32 at `CP_CRC_OFF`).
pub(crate) const CPJ_MAGIC: u32 = 0x314A_504C;
pub(crate) const CPJ_MAGIC_OFF: usize = 3072;
const CPJ_BLOCKS_OFF: usize = 3076;
const CPJ_COUNT_OFF: usize = 3080;
const CPJ_CRC_OFF: usize = 3084;
const CPJ_NEXT_OFF: usize = 3088;
const CPJ_EXT_OFF: usize = 3092;
const CPJ_EXT_MAX: usize = 64;
/// Unknown-feature mask (lane f2fsrf): a u32 after the extent list. Bit 0 =
/// "LRF1 roll-forward records may be present" (informational). A kernel that
/// finds a set bit outside `CPJ_INCOMPAT_KNOWN` mounts the volume read-only.
/// Kernels older than this lane ignore the field.
pub(crate) const CPJ_INCOMPAT_OFF: usize = 3360;
// The own-heads field (lane f2fscrc3) sits between the extension list and the incompat word.
const _: () = assert!(CPJ_EXT_OFF + 4 * CPJ_EXT_MAX <= crate::CP_OWN_HEADS && crate::CP_OWN_HEADS + 12 <= CPJ_INCOMPAT_OFF);
pub(crate) const CPJ_INCOMPAT_RF: u32 = 1;
pub(crate) const CPJ_INCOMPAT_KNOWN: u32 = CPJ_INCOMPAT_RF;
// Every dirty/write-back slot fits: the cache holds at most 2 * CACHE_MAX_SLOTS
// slots, the pack at least 511 journal blocks, an extension 512 (lane f2fsfu).
const _: () = assert!(2 * crate::CACHE_MAX_SLOTS + (2 * crate::CACHE_MAX_SLOTS + JIX_PER_BLK - 1) / JIX_PER_BLK
                      <= 511 + CPJ_EXT_MAX * 512);
pub(crate) const CP_CRC_OFF: usize = 4092;
/// Linux layout (lane f2fscrc, L12 #46): `checksum_offset` holds
/// CP_CHKSUM_OFFSET (4092) and the CRC at `CP_CRC_OFF` is Linux's f2fs_crc32
/// (crc32_le seeded with the superblock magic, no inversion). A commit block
/// whose field is 0 was written before this lane: zlib CRC32 there, and its
/// journal and roll-forward records may use the whole pack.
pub(crate) const CP_CHKSUM_FIELD: usize = 164;
/// Blocks at the end of every pack this kernel's journal and records stay out
/// of (lane f2fscrc4, L12 #46). Linux's validate_checkpoint wants
/// `F2FS_CP_PACKS < cp_pack_total_block_count <= blocks_per_seg` and a copy
/// of the commit block (same version, valid CRC) as the pack's last block,
/// and restore_curseg_summaries reads normal summaries from the blocks just
/// before it (data at total-7..-5 and node at total-4..-2 on an unmount
/// checkpoint, data at total-4..-2 otherwise). So the commit block says
/// total = the whole pack, the last block is the footer, and the six blocks
/// before it stay zero (empty summaries and journals).
pub(crate) const LINUX_TAIL: u32 = 7;
/// Lane f2fscrc packs before f2fscrc4 said total = 1 and kept six blocks free.
pub(crate) const LINUX_TAIL_V1: u32 = 6;
/// The tail the pack of commit block `b` reserves (0: pre-f2fscrc, whole pack).
pub(crate) fn cp_tail(b: &[u8; BLOCK_SIZE]) -> u32 {
    if cp_is_legacy(b) { 0 } else if r32(b, crate::CP_PACK_TOTAL) <= 2 { LINUX_TAIL_V1 } else { LINUX_TAIL }
}
/// Linux's f2fs_crc32 over `data`.
pub(crate) fn linux_crc(data: &[u8]) -> u32 { !crc32_upd(!F2FS_MAGIC, data) }
/// Commit block written before lane f2fscrc (zlib CRC, full-size pack)?
pub(crate) fn cp_is_legacy(b: &[u8; BLOCK_SIZE]) -> bool { r32(b, CP_CHKSUM_FIELD) != CP_CRC_OFF as u32 }
/// Stamp `checksum_offset` and the Linux CRC into a finished commit block.
pub(crate) fn cp_seal(b: &mut [u8; BLOCK_SIZE]) {
    w32(b, CP_CHKSUM_FIELD, CP_CRC_OFF as u32);
    let c = linux_crc(&b[..CP_CRC_OFF]);
    w32(b, CP_CRC_OFF, c);
}
/// Orphan list (lane f2fsorphan): a u32 count at `CPJ_ORPHAN_CNT_OFF`, then
/// the nids of inodes whose last name was gone while a descriptor or mapping
/// still held them when the checkpoint was cut (Linux's orphan area). Inside
/// the commit block, so under its CRC. The mount reaps every listed inode
/// that is still an unlinked inode. No feature bit: a kernel older than this
/// lane ignores the field (it only leaks such an inode, as before) and writes
/// zeroes there.
const CPJ_ORPHAN_CNT_OFF: usize = 3364;
const CPJ_ORPHAN_OFF: usize = 3368;
pub(crate) const CPJ_ORPHAN_MAX: usize = (CP_CRC_OFF - CPJ_ORPHAN_OFF) / 4;

/// Write the mount's orphan list into commit block `cp` (at most
/// `CPJ_ORPHAN_MAX`; the rest leak on a crash, as every orphan did before).
pub(crate) fn put_orphans(ms: &MountState, cp: &mut [u8; BLOCK_SIZE]) {
    let n = ms.orphans.len().min(CPJ_ORPHAN_MAX);
    w32(cp, CPJ_ORPHAN_CNT_OFF, n as u32);
    for (q, &ino) in ms.orphans[..n].iter().enumerate() { w32(cp, CPJ_ORPHAN_OFF + 4 * q, ino); }
}

/// The orphan list of commit block `cp` (empty for a pre-LPJ1 pack).
pub(crate) fn orphans_of(cp: &[u8; BLOCK_SIZE]) -> Vec<u32> {
    if r32(cp, CPJ_MAGIC_OFF) != CPJ_MAGIC { return Vec::new(); }
    let n = (r32(cp, CPJ_ORPHAN_CNT_OFF) as usize).min(CPJ_ORPHAN_MAX);
    (0..n).map(|q| r32(cp, CPJ_ORPHAN_OFF + 4 * q)).collect()
}

/// Journal index block: magic, entries in this block, then home addresses.
const JIX_MAGIC: u32 = 0x5849_4A4C; // "LJIX"
const JIX_HDR: usize = 16;
const JIX_PER_BLK: usize = (BLOCK_SIZE - JIX_HDR) / 4;

/// Roll-forward record header.
const RF_MAGIC: u32 = 0x3146_524C; // "LRF1"
const RF_FLAG_DENTRY: u32 = 1;
const RF_HDR: usize = 64;
const RF_MAX_NODES: usize = (BLOCK_SIZE - RF_HDR) / 8;

// ── CRC32 (IEEE, as Python's zlib.crc32) ─────────────────────────────────────

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}
static CRC_TABLE: [u32; 256] = crc_table();

pub(crate) fn crc32_upd(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data { c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8); }
    !c
}

/// A commit block with the journal extension: is its CRC right?
pub(crate) fn cp_block_valid(b: &[u8; BLOCK_SIZE]) -> bool {
    let want = r32(b, CP_CRC_OFF);
    if cp_is_legacy(b) { crc32_upd(0, &b[..CP_CRC_OFF]) == want } else { linux_crc(&b[..CP_CRC_OFF]) == want }
}

pub(crate) fn cp_block_has_journal(b: &[u8; BLOCK_SIZE]) -> bool {
    r32(b, CPJ_MAGIC_OFF) == CPJ_MAGIC
}

// ── Switches and the crash-test hook ─────────────────────────────────────────

/// `f2fs.rf=0`: fsync takes a full checkpoint every time (the pre-f2fsync
/// behaviour), for A/B timing.
pub(crate) fn rf_enabled() -> bool {
    static S: atomic::AtomicU8 = atomic::AtomicU8::new(0);
    match S.load(atomic::Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = drivers::nvidia_probe::cmdline_opt("f2fs.rf").map_or(true, |v| v != 0);
            S.store(if on { 1 } else { 2 }, atomic::Ordering::Relaxed);
            if !on { dbg_str("[F2FS] f2fs.rf=0: every fsync takes a full checkpoint\n"); }
            on
        }
    }
}

/// `f2fs.crashpt=N` (crash-consistency test only): on the volume mounted
/// under /mnt, every device write after the N-th block is dropped, as if the
/// power failed there, and reported as failed (so is every flush), so no
/// fsync after that point claims success; "[F2FS] crashpoint" is logged
/// once and the test harness kills the machine.
static CRASH_DEV: atomic::AtomicUsize = atomic::AtomicUsize::new(usize::MAX);
static CRASH_LEFT: atomic::AtomicU64 = atomic::AtomicU64::new(u64::MAX);
static CRASH_HIT: atomic::AtomicBool = atomic::AtomicBool::new(false);

/// fsync fell back to a full checkpoint because the roll-forward area is full
/// or the durable checkpoint left none (lane f2fsfu, L4.11). Perf only.
/// Set by `fsync_rf` when the roll-forward area passes 75%: the handler then
/// kicks the background checkpoint so fsync rarely meets a full area.
pub(crate) static RF_KICK: atomic::AtomicBool = atomic::AtomicBool::new(false);
static RF_KICKS: atomic::AtomicU64 = atomic::AtomicU64::new(0);
pub(crate) fn rf_high(ms: &MountState) -> bool {
    let d = &ms.dur;
    d.rf_end > d.rf_start && d.rf_next >= d.rf_start
        && (d.rf_next - d.rf_start) as u64 * 4 >= (d.rf_end - d.rf_start) as u64 * 3
}
static RF_FULL: atomic::AtomicU64 = atomic::AtomicU64::new(0);
fn rf_full_note() {
    let n = RF_FULL.fetch_add(1, atomic::Ordering::Relaxed);
    if n == 0 || n.is_power_of_two() {
        dbg_str("[F2FS] fsync: roll-forward area full, checkpointing instead (count ");
        dbg_dec((n + 1) as usize);
        dbg_str(")\n");
    }
}

/// `f2fs.cpfail=1` (test only): on the volume mounted under /mnt every third
/// `cp_prepare` fails as if memory or journal space had run out, which forces
/// the emergency (zero-copy) commit path.
pub(crate) static EVICT_UNFLUSHED: atomic::AtomicBool = atomic::AtomicBool::new(false);
static CPFAIL_DEV: atomic::AtomicUsize = atomic::AtomicUsize::new(usize::MAX);
static CPFAIL_N: atomic::AtomicU64 = atomic::AtomicU64::new(0);

pub(crate) fn crash_arm(dev: usize, mount_point: &str) {
    if !mount_point.starts_with("/mnt/") { return; }
    if drivers::nvidia_probe::cmdline_opt("f2fs.cpfail") == Some(1) {
        CPFAIL_DEV.store(dev, atomic::Ordering::SeqCst);
        dbg_str("[F2FS] cpfail test armed: every 3rd checkpoint snapshot fails\n");
    }
    // `f2fs.crashpt_mount=K`: arm at the K-th such mount of this boot
    // (default 1), so the harness can check the previous crash first.
    static MOUNTS: atomic::AtomicU64 = atomic::AtomicU64::new(0);
    let k = MOUNTS.fetch_add(1, atomic::Ordering::SeqCst) + 1;
    if k != drivers::nvidia_probe::cmdline_opt("f2fs.crashpt_mount").unwrap_or(1) {
        if CRASH_DEV.load(atomic::Ordering::SeqCst) == dev { CRASH_DEV.store(usize::MAX, atomic::Ordering::SeqCst); }
        return;
    }
    if let Some(n) = drivers::nvidia_probe::cmdline_opt("f2fs.crashpt") {
        CRASH_LEFT.store(n, atomic::Ordering::SeqCst);
        CRASH_DEV.store(dev, atomic::Ordering::SeqCst);
        CRASH_HIT.store(false, atomic::Ordering::SeqCst);
        dbg_str("[F2FS] crash test armed: writes stop after f2fs.crashpt device writes\n");
    }
}

/// False: the write must be dropped and fail (the simulated power cut).
pub(crate) fn crash_pass(dev: usize, nblocks: u64) -> bool {
    if CRASH_DEV.load(atomic::Ordering::Relaxed) != dev { return true; }
    let left = CRASH_LEFT.load(atomic::Ordering::SeqCst);
    if left >= nblocks {
        CRASH_LEFT.store(left - nblocks, atomic::Ordering::SeqCst);
        return true;
    }
    CRASH_LEFT.store(0, atomic::Ordering::SeqCst);
    if !CRASH_HIT.swap(true, atomic::Ordering::SeqCst) {
        dbg_str("[F2FS] crashpoint reached: further writes to the test volume are dropped\n");
    }
    false
}

pub(crate) fn crash_dead(dev: usize) -> bool {
    CRASH_DEV.load(atomic::Ordering::Relaxed) == dev && CRASH_HIT.load(atomic::Ordering::SeqCst)
}

fn dw(dev: usize, blk: u64, buf: &[u8; BLOCK_SIZE]) -> bool {
    if !crash_pass(dev, 1) { return false; }
    virtio_blk::write_block(dev, blk, buf)
}

/// Write `data` (whole blocks) at `blk` in requests of `WB_IO_BLOCKS`.
/// Write a pack's Linux tail at `at` (LINUX_TAIL blocks): six zero summary
/// blocks when `zero`, and always the footer copy of the sealed commit block.
fn write_tail(dev: usize, at: u64, cp: &[u8; BLOCK_SIZE], zero: bool, cmds: &mut u32) -> bool {
    if !zero { *cmds += 1; return dw(dev, at + LINUX_TAIL as u64 - 1, cp); }
    let mut t = alloc::vec![0u8; LINUX_TAIL as usize * BLOCK_SIZE];
    t[(LINUX_TAIL as usize - 1) * BLOCK_SIZE..].copy_from_slice(cp);
    dws(dev, at, &t, cmds)
}

fn dws(dev: usize, blk: u64, data: &[u8], cmds: &mut u32) -> bool {
    let mut ok = true;
    for (p, piece) in data.chunks(WB_IO_BLOCKS * BLOCK_SIZE).enumerate() {
        let at = blk + (p * WB_IO_BLOCKS) as u64;
        *cmds += 1;
        if !crash_pass(dev, (piece.len() / BLOCK_SIZE) as u64) { ok = false; continue; }
        if piece.len() == BLOCK_SIZE {
            ok &= virtio_blk::write_block(dev, at, piece.try_into().unwrap());
        } else if !virtio_blk::write_blocks(dev, at, piece) {
            for (j, b) in piece.chunks_exact(BLOCK_SIZE).enumerate() {
                ok &= virtio_blk::write_block(dev, at + j as u64, b.try_into().unwrap());
            }
        }
    }
    ok
}

fn dflush(dev: usize) -> bool {
    if crash_dead(dev) { return false; }
    virtio_blk::flush(dev)
}

// ── Per-mount state ──────────────────────────────────────────────────────────

pub(crate) struct Durable {
    /// The pack's Linux tail may hold older data (a pre-f2fscrc journal or
    /// records, a lane-f2fscrc V1 footer gap): zeroed by its first commit.
    pub tail_dirty: [bool; 2],
    /// Main-area blocks allocated since the last checkpoint snapshot (free
    /// in the durable checkpoint): written in place before the commit.
    pub fresh_pending: BTreeMap<u32, SegMap>,
    /// The same, up to the snapshot of the checkpoint in flight.
    pub fresh_inflight: BTreeMap<u32, SegMap>,
    /// Node ids freed since the last snapshot (NAT entry cleared).
    pub nid_pending: BTreeSet<u32>,
    /// Ids freed before the snapshot of the checkpoint in flight.
    pub nid_inflight: Vec<u32>,
    /// Ids whose free is durable: handed out first.
    pub nid_pool: Vec<u32>,
    /// The prepared checkpoint not yet settled (0 = none) and where it goes.
    pub inflight_ver: u64,
    pub inflight_pack: u8,
    pub inflight_rf_start: u32,
    /// The newest checkpoint known to be on the medium.
    pub durable_ver: u64,
    pub durable_pack: u8,
    /// Roll-forward area of the durable pack (pack-relative blocks): the
    /// next record goes at `rf_next`; `rf_end == 0`: no room (fsync then
    /// checkpoints).
    pub rf_next: u32,
    pub rf_end: u32,
    /// First block of the roll-forward area (for the 75% background trigger).
    pub rf_start: u32,
    pub rf_seq: u32,
    /// Namespace changes since the checkpoint numbered `.0` started:
    /// `.1` = the only change is the file's creation (dentry mark).
    pub ns: BTreeMap<u32, (u64, bool)>,
    /// CRC of each inode's last record in the current roll-forward era.
    pub rf_last: BTreeMap<u32, u32>,
    /// Free unprotected whole segments a journal could extend into (capped
    /// at CPJ_EXT_MAX), as of the last settled checkpoint (a throttle bound).
    pub ext_free: u32,
    /// Index arrays of the emergency checkpoint, preallocated at mount so it
    /// never allocates (capacity `2 * cache.cap`; never grown).
    pub emerg_a: Vec<u32>,
    pub emerg_b: Vec<u32>,
}

impl Durable {
    pub(crate) fn new(ver: u64, pack: u8) -> Self {
        Durable {
            tail_dirty: [true; 2],
            fresh_pending: BTreeMap::new(), fresh_inflight: BTreeMap::new(), nid_pending: BTreeSet::new(), nid_inflight: Vec::new(),
            nid_pool: Vec::new(), inflight_ver: 0, inflight_pack: 0, inflight_rf_start: 0,
            durable_ver: ver, durable_pack: pack, rf_next: 0, rf_end: 0, rf_start: 0, rf_seq: 0,
            ns: BTreeMap::new(), rf_last: BTreeMap::new(),
            ext_free: 0, emerg_a: Vec::new(), emerg_b: Vec::new(),
        }
    }
}

/// Committed checkpoint versions, per mount slot (set the moment the commit
/// block is on the medium, before the lock is taken again).
static COMMITTED: [atomic::AtomicU64; MAX_MOUNTS] = [const { atomic::AtomicU64::new(0) }; MAX_MOUNTS];
pub(crate) static RF_COUNT: atomic::AtomicU32 = atomic::AtomicU32::new(0);
pub(crate) static RF_FALLBACK: atomic::AtomicU32 = atomic::AtomicU32::new(0);

fn committed_slot(port: u32) -> Option<&'static atomic::AtomicU64> {
    CP_SEQ.iter().position(|s| s.port.load(atomic::Ordering::Acquire) == port).map(|i| &COMMITTED[i])
}

pub(crate) fn committed_init(slot: usize, ver: u64) { COMMITTED[slot].store(ver, atomic::Ordering::SeqCst); }

fn pack_blocks(ms: &MountState) -> u32 { (ms.sb.seg_cnt_ckpt / 2) * ms.sb.blocks_per_seg }
/// The part of a pack this kernel writes (journal, roll-forward records).
fn pack_room(ms: &MountState) -> u32 { pack_blocks(ms) - LINUX_TAIL }
fn pack_base(ms: &MountState, pack: u8) -> u64 { (ms.sb.cp_blkaddr + pack as u32 * pack_blocks(ms)) as u64 }

/// A block was allocated (log_alloc).
pub(crate) fn note_alloc(ms: &mut MountState, seg: u32, b: u32) {
    if b >= 512 { return; }
    let e = ms.dur.fresh_pending.entry(seg).or_insert([0; 8]);
    e[b as usize / 64] |= 1u64 << (b % 64);
}

/// Does the SIT (as cached now) mark main-area block `blk` valid?
fn sit_valid(ms: &mut MountState, blk: u64) -> bool {
    let Some((seg, b)) = blkaddr_to_seg(ms, blk as u32) else { return true };
    if seg >= ms.sb.seg_cnt_main { return true; }
    let (blkno, off) = sit_entry_loc(ms, seg);
    let sit = ms.cache.read(ms.dev, blkno);
    sit[off + SIT_VMAP_OFF + b as usize / 8] & (1 << (b % 8)) != 0
}

fn is_fresh(ms: &MountState, blk: u64) -> bool {
    let Some((seg, b)) = blkaddr_to_seg(ms, blk as u32) else { return false };
    ms.dur.fresh_pending.get(&seg).map_or(false, |m| m[b as usize / 64] & (1u64 << (b % 64)) != 0)
}

/// Namespace change of `ino` (`created`: its creation).
pub(crate) fn ns_note(ms: &mut MountState, ino: u32, created: bool) {
    if ino == 0 { return; }
    let n = next_cp_seq(ms.port);
    let e = ms.dur.ns.entry(ino).or_insert((n, created));
    if !(created && e.1) { e.1 = false; }
    e.0 = e.0.max(n);
}

// ── Node ids ─────────────────────────────────────────────────────────────────

/// Clear `nid`'s NAT entry (the node is gone) and queue the id for reuse.
pub(crate) fn node_freed(ms: &mut MountState, nid: u32) {
    if nid == 0 || nid <= ms.sb.root_ino { return; }
    nat_clear(ms, nid);
    if !ms.cache.poisoned() { ms.dur.nid_pending.insert(nid); }
}

fn nat_clear(ms: &mut MountState, nid: u32) {
    let nat_blk = ms.sb.nat_blkaddr + nid / NAT_ENTRY_PER_BLK as u32;
    let off = (nid % NAT_ENTRY_PER_BLK as u32) as usize * NAT_ENTRY_SIZE;
    {
        let cur = ms.cache.read(ms.dev, nat_blk as u64);
        if r32(cur, off + 1) == 0 && r32(cur, off + 5) == 0 { return; }
    }
    let blk = ms.cache.get_mut(ms.dev, nat_blk as u64);
    w32(blk, off + 1, 0);
    w32(blk, off + 5, 0);
}

/// `f2fs.nid_max=N` (tests; volumes mounted under /mnt only): the NAT is
/// treated as N entries long.
fn nid_cap() -> u64 {
    static CAP: atomic::AtomicU64 = atomic::AtomicU64::new(0);
    let c = CAP.load(atomic::Ordering::Relaxed);
    if c != 0 { return c; }
    let v = drivers::nvidia_probe::cmdline_opt("f2fs.nid_max").filter(|&v| v > 0).unwrap_or(u64::MAX);
    CAP.store(v, atomic::Ordering::Relaxed);
    v
}

/// The next node id: a recycled one, else the next never-used one, else
/// (NAT exhausted) one found free by scanning the NAT. None: ENOSPC.
pub(crate) fn take_nid(ms: &mut MountState) -> Option<u32> {
    while let Some(n) = ms.dur.nid_pool.pop() {
        if !ms.dur.nid_pending.contains(&n) && nat_lookup(ms, n) == 0 { return Some(n); }
    }
    let max = (ms.sb.seg_cnt_nat as u64 * ms.sb.blocks_per_seg as u64 * NAT_ENTRY_PER_BLK as u64)
        .min(u32::MAX as u64).min(if ms.mount_prefix.starts_with("/mnt/") { nid_cap() } else { u64::MAX });
    let nid = ms.cp.next_free_nid;
    if (nid as u64) < max {
        ms.cp.next_free_nid = nid + 1;
        return Some(nid);
    }
    // Every id was handed out once: look for ones freed by a durable
    // checkpoint (NAT entry 0, not freed since the last snapshot).
    if !ms.cache.poisoned() {
        let first = ms.sb.root_ino + 1;
        let inflight: BTreeSet<u32> = ms.dur.nid_inflight.iter().copied().collect();
        let mut found = Vec::new();
        let mut n = first;
        while n < ms.cp.next_free_nid && found.len() < 256 {
            if nat_lookup(ms, n) == 0 && !ms.dur.nid_pending.contains(&n) && !inflight.contains(&n) {
                found.push(n);
            }
            if ms.cache.poisoned() { return None; }
            n += 1;
        }
        if let Some(&n) = found.first() {
            ms.dur.nid_pool.extend(found.into_iter().skip(1).rev());
            return Some(n);
        }
    }
    // Ids freed since the last checkpoint become usable once one commits:
    // doom the request like a space shortage (`with_mount` checkpoints and
    // runs it again).
    if !ms.cache.poisoned() && (!ms.dur.nid_pending.is_empty() || !ms.dur.nid_inflight.is_empty()) {
        if ms.cache.probe == PROBE_ON && CP_MUTEX.owner_raw() != sched::current_pid() as u32 + 1 {
            ms.space_short = true;
            ms.cache.doom();
            return None;
        }
    }
    static REPORTED: atomic::AtomicBool = atomic::AtomicBool::new(false);
    if !ms.cache.poisoned() && !REPORTED.swap(true, atomic::Ordering::Relaxed) {
        dbg_str("[F2FS] out of node ids (every NAT entry is in use): ENOSPC\n");
    }
    None
}

// ── Checkpoint transaction ───────────────────────────────────────────────────

pub(crate) struct CpJob {
    pub dev: usize,
    pub ver: u64,
    /// Written in place before the commit (block order, adjacent merged).
    pub direct: Vec<(u64, Vec<u8>)>,
    pub direct_reg: Vec<u8>,
    /// Journaled blocks: home addresses (ascending) and their images.
    pub jhome: Vec<u32>,
    pub jimg: Vec<u8>,
    /// Journal placement: pack base (the commit block's address), spare
    /// blocks in the pack, extension segments (first block each).
    pub pack_base: u64,
    pub pack_cap: u32,
    pub ext: Vec<u64>,
    pub cp_blkno: u64,
    pub cp_buf: [u8; BLOCK_SIZE],
    pub umount: bool,
    /// Zero the six summary blocks of the Linux tail with this commit (the
    /// footer copy of the commit block is written every time).
    pub zero_tail: bool,
    pub reg_blocks: [u32; REGS],
}

/// Resolve the checkpoint prepared last: committed (its frees, fresh
/// blocks and ids are durable; its pack is the durable one and its spare
/// blocks the roll-forward area) or failed (everything goes back to
/// pending, so the next checkpoint commits it). Idempotent.
pub(crate) fn settle(ms: &mut MountState) {
    let v = ms.dur.inflight_ver;
    if v == 0 { return; }
    ms.dur.inflight_ver = 0;
    let committed = committed_slot(ms.port).map_or(false, |c| c.load(atomic::Ordering::SeqCst) >= v);
    if committed {
        ms.prot_inflight.clear();
        let ids = core::mem::take(&mut ms.dur.nid_inflight);
        ms.dur.nid_pool.extend(ids);
        ms.dur.durable_ver = v;
        ms.dur.durable_pack = ms.dur.inflight_pack;
        let st = ms.dur.inflight_rf_start;
        ms.dur.rf_next = st;
        ms.dur.rf_start = st;
        ms.dur.rf_end = if st == 0 { 0 } else { pack_room(ms) };
        ms.dur.rf_seq = 0;
        ms.dur.rf_last.clear();
        let done = cp_seq(ms.port).map_or(0, |s| s.done.load(atomic::Ordering::SeqCst));
        ms.dur.ns.retain(|_, e| e.0 > done);
        ms.dur.ext_free = count_ext_free(ms);
    } else {
        prot_committed(ms, false);
        let ids = core::mem::take(&mut ms.dur.nid_inflight);
        ms.dur.nid_pending.extend(ids);
        // The fresh blocks of the failed snapshot are still fresh.
        let f = core::mem::take(&mut ms.dur.fresh_inflight);
        for (seg, m) in f {
            let e = ms.dur.fresh_pending.entry(seg).or_insert([0; 8]);
            for w in 0..8 { e[w] |= m[w]; }
        }
    }
    ms.dur.fresh_inflight.clear();
}

/// Free unprotected whole segments (a journal's extension candidates), up to
/// CPJ_EXT_MAX. The scan stops at the cap, so it is short unless the volume is
/// nearly full.
pub(crate) fn count_ext_free(ms: &mut MountState) -> u32 {
    let mut cnt = 0u32;
    let mut seg = ms.sb.seg_cnt_main;
    while (cnt as usize) < CPJ_EXT_MAX && seg > 0 {
        seg -= 1;
        if seg == ms.cp.cur_data_segno || seg == ms.cp.cur_node_segno || prot_any(ms, seg) { continue; }
        let (blkno, off) = sit_entry_loc(ms, seg);
        if r16(ms.cache.read(ms.dev, blkno), off) & SIT_VBLOCKS_MASK == 0 { cnt += 1; }
    }
    cnt
}

/// Whole free segments for a journal that does not fit in its pack: no
/// valid block, nothing protected, not a log's segment.
fn pick_ext(ms: &mut MountState, out: &mut [u32; CPJ_EXT_MAX], n: usize) -> bool {
    let mut cnt = 0;
    let mut seg = ms.sb.seg_cnt_main;
    while cnt < n && seg > 0 {
        seg -= 1;
        if seg == ms.cp.cur_data_segno || seg == ms.cp.cur_node_segno || prot_any(ms, seg) { continue; }
        let (blkno, off) = sit_entry_loc(ms, seg);
        let vb = r16(ms.cache.read(ms.dev, blkno), off) & SIT_VBLOCKS_MASK;
        if vb == 0 { out[cnt] = seg; cnt += 1; }
    }
    cnt == n
}

/// Upper bound of the blocks a checkpoint journal can hold: the pack's spare
/// blocks plus half the (stale-high) free whole segments, 512 blocks each.
pub(crate) fn journal_room(ms: &MountState) -> usize {
    (pack_room(ms) as usize).saturating_sub(1) + 512 * (ms.dur.ext_free as usize / 2)
}

/// Phase 1 (lock held): snapshot every dirty slot (and, `with_wb`, every
/// slot in write-back) into a job and mark them clean-but-pinned. Err if
/// memory or journal space ran out (nothing changed then).
pub(crate) fn cp_prepare(ms: &mut MountState, umount: bool, with_wb: bool) -> Result<CpJob, ()> {
    settle(ms);
    if CPFAIL_DEV.load(atomic::Ordering::Relaxed) == ms.dev
        && CPFAIL_N.fetch_add(1, atomic::Ordering::Relaxed) % 3 == 2 { return Err(()); }
    let c = &ms.cache;
    let mut idx: Vec<usize> = Vec::new();
    idx.try_reserve_exact(c.npinned).map_err(|_| ())?;
    idx.extend((0..c.meta.len()).filter(|&i| c.meta[i].dirty || (with_wb && c.meta[i].wb)));
    idx.sort_unstable_by_key(|&i| c.meta[i].blk_no);
    let main = ms.sb.main_blkaddr as u64;
    let cp_lo = ms.sb.cp_blkaddr as u64;
    let cp_hi = cp_lo + 2 * pack_blocks(ms) as u64;
    // Classify.
    let mut direct_idx: Vec<usize> = Vec::new();
    let mut jidx: Vec<usize> = Vec::new();
    let mut dead: Vec<usize> = Vec::new();
    direct_idx.try_reserve_exact(idx.len()).map_err(|_| ())?;
    jidx.try_reserve_exact(idx.len()).map_err(|_| ())?;
    dead.try_reserve_exact(idx.len()).map_err(|_| ())?;
    for &i in &idx {
        let m = ms.cache.meta[i];
        if m.blk_no >= cp_lo && m.blk_no < cp_hi { continue; } // never cached; never written here
        // A main-area block the new checkpoint marks free (freed since it
        // was dirtied): its contents mean nothing. Not written at all: it
        // may still be valid in the durable checkpoint, and a journaled
        // copy would be replayed over its next owner after a crash.
        if m.blk_no >= main && !sit_valid(ms, m.blk_no) { dead.push(i); continue; }
        if m.blk_no >= main && (m.udata || is_fresh(ms, m.blk_no)) { direct_idx.push(i); } else { jidx.push(i); }
    }
    let j = jidx.len();
    let h = (j + JIX_PER_BLK - 1) / JIX_PER_BLK;
    let total = (h + j) as u32;
    let pack_cap = pack_room(ms) - 1;
    let ext_n = if total > pack_cap { ((total - pack_cap) as usize + 511) / 512 } else { 0 };
    if ext_n > CPJ_EXT_MAX { return Err(()); }
    let mut ext_arr = [0u32; CPJ_EXT_MAX];
    if ext_n > 0 && !pick_ext(ms, &mut ext_arr, ext_n) { return Err(()); }
    let ext_segs = &ext_arr[..ext_n];
    let logs = crate::linux_logs(ms, ext_segs);
    // Buffers (all allocations before any slot changes).
    let mut jimg: Vec<u8> = Vec::new();
    jimg.try_reserve_exact(j * BLOCK_SIZE).map_err(|_| ())?;
    let mut jhome: Vec<u32> = Vec::new();
    jhome.try_reserve_exact(j).map_err(|_| ())?;
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let mut k = 0;
    while k < direct_idx.len() {
        let mut n = 1;
        while k + n < direct_idx.len() && n < MAX_RUN
            && ms.cache.meta[direct_idx[k + n]].blk_no == ms.cache.meta[direct_idx[k]].blk_no + n as u64 { n += 1; }
        bounds.try_reserve(1).map_err(|_| ())?;
        bounds.push((k, n));
        k += n;
    }
    let mut direct: Vec<(u64, Vec<u8>)> = Vec::new();
    direct.try_reserve_exact(bounds.len()).map_err(|_| ())?;
    let mut direct_reg: Vec<u8> = Vec::new();
    direct_reg.try_reserve_exact(bounds.len()).map_err(|_| ())?;
    for &(k, n) in &bounds {
        let mut v: Vec<u8> = Vec::new();
        v.try_reserve_exact(n * BLOCK_SIZE).map_err(|_| ())?;
        direct.push((ms.cache.meta[direct_idx[k]].blk_no, v));
    }
    // Copy and pin.
    for &i in &dead { ms.cache.set_flags(i, false, false); }
    let mut reg_blocks = [0u32; REGS];
    for (r, &(k, n)) in bounds.iter().enumerate() {
        for q in 0..n {
            let i = direct_idx[k + q];
            let reg = block_region(ms, ms.cache.meta[i].blk_no, ms.cache.data(i));
            reg_blocks[reg] += 1;
            if q == 0 { direct_reg.push(reg as u8); }
            direct[r].1.extend_from_slice(ms.cache.data(i));
            ms.cache.set_flags(i, false, true);
        }
    }
    for &i in &jidx {
        let reg = block_region(ms, ms.cache.meta[i].blk_no, ms.cache.data(i));
        reg_blocks[reg] += 1;
        jhome.push(ms.cache.meta[i].blk_no as u32);
        jimg.extend_from_slice(ms.cache.data(i));
        ms.cache.set_flags(i, false, true);
    }
    // Commit block.
    let pack = 1 - ms.dur.durable_pack;
    let (cp_blkno, mut cp_buf) = build_cp_block(ms, umount, pack, &logs);
    w32(&mut cp_buf, CPJ_MAGIC_OFF, CPJ_MAGIC);
    w32(&mut cp_buf, CPJ_INCOMPAT_OFF, if rf_enabled() { CPJ_INCOMPAT_RF } else { 0 });
    put_orphans(ms, &mut cp_buf);
    w32(&mut cp_buf, CPJ_BLOCKS_OFF, total);
    w32(&mut cp_buf, CPJ_COUNT_OFF, j as u32);
    w32(&mut cp_buf, CPJ_NEXT_OFF, ext_segs.len() as u32);
    for (q, &s) in ext_segs.iter().enumerate() { w32(&mut cp_buf, CPJ_EXT_OFF + 4 * q, s); }
    let rf_start = if ext_n == 0 && 1 + total < pack_room(ms) { 1 + total } else { 0 };
    // The snapshot: frees, fresh blocks and ids up to now belong to it.
    prot_snapshot(ms);
    for &s in ext_segs {
        let e = ms.prot_pending.entry(s).or_insert([0; 8]);
        *e = [u64::MAX; 8];
    }
    ms.dur.fresh_inflight = core::mem::take(&mut ms.dur.fresh_pending);
    let pend = core::mem::take(&mut ms.dur.nid_pending);
    for n in pend {
        if nat_lookup(ms, n) == 0 { ms.dur.nid_inflight.push(n); }
    }
    ms.dur.inflight_ver = ms.cp.ver;
    ms.dur.inflight_pack = pack;
    ms.dur.inflight_rf_start = rf_start;
    let ext = ext_segs.iter().map(|&s| ms.sb.main_blkaddr as u64 + s as u64 * ms.sb.blocks_per_seg as u64).collect();
    let zero_tail = core::mem::replace(&mut ms.dur.tail_dirty[pack as usize], false);
    Ok(CpJob {
        dev: ms.dev, ver: ms.cp.ver, direct, direct_reg, jhome, jimg,
        pack_base: pack_base(ms, pack), pack_cap, ext, cp_blkno, cp_buf, umount, reg_blocks, zero_tail,
    })
}

/// Journal logical block `k` (index blocks first, then images) -> device block.
fn jaddr(job: &CpJob, k: u32) -> u64 { jaddr_raw(job.pack_base, job.pack_cap, &job.ext, k) }

fn jaddr_raw(pack_base: u64, pack_cap: u32, ext: &[u64], k: u32) -> u64 {
    if k < pack_cap { pack_base + 1 + k as u64 } else {
        let e = (k - pack_cap) as u64;
        ext[(e / 512) as usize] + e % 512
    }
}

/// Write `data` as journal blocks from logical `k0`, split at area ends.
fn jwrite(job: &CpJob, k0: u32, data: &[u8], cmds: &mut u32) -> bool {
    let mut ok = true;
    let mut k = k0;
    let mut off = 0usize;
    let n = (data.len() / BLOCK_SIZE) as u32;
    while k < k0 + n {
        let room = if k < job.pack_cap { job.pack_cap - k } else { 512 - (k - job.pack_cap) % 512 };
        let take = room.min(k0 + n - k);
        let bytes = take as usize * BLOCK_SIZE;
        ok &= dws(job.dev, jaddr(job, k), &data[off..off + bytes], cmds);
        off += bytes;
        k += take;
    }
    ok
}

pub(crate) struct CpIo { pub ok: bool, pub home_ok: bool, pub reg_ns: [u64; REGS], pub reg_cmds: [u32; REGS], pub flush_ns: u64 }

/// Phase 2 (no filesystem lock needed): direct blocks and the journal, flush,
/// the commit block, flush, then the journaled blocks home.
pub(crate) fn cp_write(job: &mut CpJob, port: u32) -> CpIo {
    let dev = job.dev;
    let mut io = CpIo { ok: true, home_ok: true, reg_ns: [0; REGS], reg_cmds: [0; REGS], flush_ns: 0 };
    for (r, (start, data)) in job.direct.iter().enumerate() {
        let reg = job.direct_reg.get(r).map_or(REG_DATA, |&g| g as usize);
        let t = sched::monotonic_ns();
        io.ok &= dws(dev, *start, data, &mut io.reg_cmds[reg]);
        io.reg_ns[reg] += sched::monotonic_ns().saturating_sub(t);
    }
    let j = job.jhome.len();
    let t = sched::monotonic_ns();
    let mut crc = 0u32;
    if j > 0 {
        let mut ix = [0u8; BLOCK_SIZE];
        let mut k = 0u32;
        for chunk in job.jhome.chunks(JIX_PER_BLK) {
            ix.fill(0);
            w32(&mut ix, 0, JIX_MAGIC);
            w32(&mut ix, 4, chunk.len() as u32);
            w64(&mut ix, 8, job.ver);
            for (q, &a) in chunk.iter().enumerate() { w32(&mut ix, JIX_HDR + 4 * q, a); }
            crc = crc32_upd(crc, &ix);
            io.ok &= jwrite(job, k, &ix, &mut io.reg_cmds[REG_CP]);
            k += 1;
        }
        crc = crc32_upd(crc, &job.jimg);
        io.ok &= jwrite(job, k, &job.jimg, &mut io.reg_cmds[REG_CP]);
    }
    io.reg_ns[REG_CP] += sched::monotonic_ns().saturating_sub(t);
    let tf = sched::monotonic_ns();
    io.ok &= dflush(dev);
    io.flush_ns += sched::monotonic_ns().saturating_sub(tf);
    if !io.ok { io.home_ok = false; return io; }
    w32(&mut job.cp_buf, CPJ_CRC_OFF, crc);
    cp_seal(&mut job.cp_buf);
    let t = sched::monotonic_ns();
    io.ok &= dw(dev, job.cp_blkno, &job.cp_buf);
    io.reg_cmds[REG_CP] += 1;
    // Linux's view of the pack, in the same flush as the commit block. This
    // kernel never reads it, so a failed write does not fail the commit.
    io.home_ok &= write_tail(dev, job.pack_base + 1 + job.pack_cap as u64, &job.cp_buf, job.zero_tail,
                             &mut io.reg_cmds[REG_CP]);
    let tf = sched::monotonic_ns();
    io.reg_ns[REG_CP] += tf.saturating_sub(t);
    io.ok &= dflush(dev);
    io.flush_ns += sched::monotonic_ns().saturating_sub(tf);
    if !io.ok { io.home_ok = false; return io; }
    if let Some(c) = committed_slot(port) { c.fetch_max(job.ver, atomic::Ordering::SeqCst); }
    // Home writes, adjacent homes merged (the images are in home order).
    let mut q = 0;
    while q < j {
        let mut n = 1;
        while q + n < j && n < MAX_RUN && job.jhome[q + n] == job.jhome[q] + n as u32 { n += 1; }
        let reg = block_region_addr(job.jhome[q] as u64);
        let t = sched::monotonic_ns();
        io.home_ok &= dws(dev, job.jhome[q] as u64, &job.jimg[q * BLOCK_SIZE..(q + n) * BLOCK_SIZE],
                          &mut io.reg_cmds[reg]);
        io.reg_ns[reg] += sched::monotonic_ns().saturating_sub(t);
        q += n;
    }
    if job.umount {
        let tf = sched::monotonic_ns();
        io.home_ok &= dflush(dev);
        io.flush_ns += sched::monotonic_ns().saturating_sub(tf);
    }
    io
}

/// Stats region of a home write (NAT/SIT/node+dentry), without the cache.
fn block_region_addr(_blk: u64) -> usize { REG_NODE }

/// Phase 3 (lock held): unpin the job's slots (dirty again if not on the
/// medium) and settle.
pub(crate) fn cp_finish(ms: &mut MountState, job: &CpJob, io: &CpIo) {
    for (start, data) in &job.direct {
        for q in 0..(data.len() / BLOCK_SIZE) as u64 {
            if let Some(i) = ms.cache.find(start + q) {
                if ms.cache.meta[i].wb {
                    let d = ms.cache.meta[i].dirty || !io.ok;
                    ms.cache.set_flags(i, d, false);
                }
            }
        }
    }
    for &h in &job.jhome {
        if let Some(i) = ms.cache.find(h as u64) {
            if ms.cache.meta[i].wb {
                let d = ms.cache.meta[i].dirty || !io.ok || !io.home_ok;
                ms.cache.set_flags(i, d, false);
            }
        }
    }
    if !io.ok { ms.dirty_writes = ms.dirty_writes.max(1); }
    settle(ms);
}

// ── Mount: journal replay ────────────────────────────────────────────────────

/// Read the journal of the pack whose commit block is `cpb` (at `base`) and
/// check its CRC. Ok((homes, images, rf_start)); Err if it is corrupt.
pub(crate) fn read_journal(dev: usize, sb: &SbInfo, base: u64, cpb: &[u8; BLOCK_SIZE])
    -> Result<(Vec<u32>, Vec<u8>, u32), ()>
{
    let pack_blks = (sb.seg_cnt_ckpt / 2) * sb.blocks_per_seg;
    if !cp_block_has_journal(cpb) { return Ok((Vec::new(), Vec::new(), 1)); }
    let total = r32(cpb, CPJ_BLOCKS_OFF);
    let j = r32(cpb, CPJ_COUNT_OFF) as usize;
    let next = r32(cpb, CPJ_NEXT_OFF) as usize;
    if next > CPJ_EXT_MAX || j > total as usize { return Err(()); }
    let ext: Vec<u64> = (0..next).map(|q| sb.main_blkaddr as u64
        + r32(cpb, CPJ_EXT_OFF + 4 * q) as u64 * sb.blocks_per_seg as u64).collect();
    let job_like = CpJob {
        dev, ver: 0, direct: Vec::new(), direct_reg: Vec::new(), jhome: Vec::new(), jimg: Vec::new(),
        pack_base: base, pack_cap: pack_blks - 1 - cp_tail(cpb), ext, cp_blkno: 0, cp_buf: [0; BLOCK_SIZE],
        umount: false, reg_blocks: [0; REGS], zero_tail: false,
    };
    if total > job_like.pack_cap + 512 * next as u32 { return Err(()); }
    let h = (j + JIX_PER_BLK - 1) / JIX_PER_BLK;
    if h + j != total as usize { return Err(()); }
    let mut crc = 0u32;
    let mut homes = Vec::new();
    let mut imgs: Vec<u8> = Vec::new();
    if imgs.try_reserve_exact(j * BLOCK_SIZE).is_err() { return Err(()); }
    let mut buf = [0u8; BLOCK_SIZE];
    for k in 0..total {
        if !virtio_blk::read_block(dev, jaddr(&job_like, k), &mut buf) { return Err(()); }
        crc = crc32_upd(crc, &buf);
        if (k as usize) < h {
            if r32(&buf, 0) != JIX_MAGIC { return Err(()); }
            let n = (r32(&buf, 4) as usize).min(JIX_PER_BLK);
            for q in 0..n { homes.push(r32(&buf, JIX_HDR + 4 * q)); }
        } else {
            imgs.extend_from_slice(&buf);
        }
    }
    if crc != r32(cpb, CPJ_CRC_OFF) || homes.len() != j { return Err(()); }
    let rf_start = if next == 0 && 1 + total < pack_blks { 1 + total } else { 0 };
    Ok((homes, imgs, rf_start))
}

/// Put the journal's images home. Read-write: write only the blocks whose
/// home differs, then flush. Read-only: install them as dirty cache blocks
/// (never written; reads see the committed state).
pub(crate) fn replay_journal(ms: &mut MountState, homes: &[u32], imgs: &[u8]) -> usize {
    let mut rewritten = 0;
    for (q, &h) in homes.iter().enumerate() {
        let img: &[u8; BLOCK_SIZE] = imgs[q * BLOCK_SIZE..(q + 1) * BLOCK_SIZE].try_into().unwrap();
        let same = { let cur = ms.cache.read(ms.dev, h as u64); cur[..] == img[..] };
        if same { continue; }
        rewritten += 1;
        if ms.read_only {
            ms.cache.write(ms.dev, h as u64, img);
        } else {
            virtio_blk::write_block(ms.dev, h as u64, img);
            if let Some(i) = ms.cache.find(h as u64) { ms.cache.data_mut(i).copy_from_slice(img); }
        }
    }
    if rewritten > 0 && !ms.read_only { virtio_blk::flush(ms.dev); }
    rewritten
}

// ── Roll-forward fsync ───────────────────────────────────────────────────────

/// The block tree of `ino` as the cache has it: (nid, address) of every node
/// (inode, xattr, dnodes, indirect nodes) and every data block address.
fn tree(ms: &mut MountState, ino: u32) -> (Vec<(u32, u32)>, Vec<u32>) {
    let mut nodes = Vec::new();
    let mut data = Vec::new();
    let ia = nat_lookup(ms, ino);
    if ia == 0 { return (nodes, data); }
    nodes.push((ino, ia));
    let ib = read_block_copy(ms, ia);
    for i in 0..inode_max_direct(&ib) {
        let a = inode_get_blkaddr(&ib, i);
        if a != 0 { data.push(a); }
    }
    let x = r32(&ib, INO_XATTR);
    if x != 0 { let a = nat_lookup(ms, x); if a != 0 { nodes.push((x, a)); } }
    const PER: usize = NODE_FOOTER_OFF / 4;
    fn dnode(ms: &mut MountState, nid: u32, nodes: &mut Vec<(u32, u32)>, data: &mut Vec<u32>) {
        if nid == 0 { return; }
        let a = nat_lookup(ms, nid);
        if a == 0 { return; }
        nodes.push((nid, a));
        let b = read_block_copy(ms, a);
        for i in 0..PER { let d = dnode_get_blkaddr(&b, i); if d != 0 { data.push(d); } }
    }
    fn indirect(ms: &mut MountState, nid: u32, nodes: &mut Vec<(u32, u32)>, data: &mut Vec<u32>) {
        if nid == 0 { return; }
        let a = nat_lookup(ms, nid);
        if a == 0 { return; }
        nodes.push((nid, a));
        let b = read_block_copy(ms, a);
        for i in 0..PER { dnode(ms, dnode_get_blkaddr(&b, i), nodes, data); }
    }
    dnode(ms, inode_get_nid(&ib, 0), &mut nodes, &mut data);
    dnode(ms, inode_get_nid(&ib, 1), &mut nodes, &mut data);
    indirect(ms, inode_get_nid(&ib, 2), &mut nodes, &mut data);
    indirect(ms, inode_get_nid(&ib, 3), &mut nodes, &mut data);
    let dn = inode_get_nid(&ib, 4);
    if dn != 0 {
        let a = nat_lookup(ms, dn);
        if a != 0 {
            nodes.push((dn, a));
            let b = read_block_copy(ms, a);
            for i in 0..PER { indirect(ms, dnode_get_blkaddr(&b, i), &mut nodes, &mut data); }
        }
    }
    (nodes, data)
}

/// fsync of regular file `ino` by roll-forward record. None: not possible
/// here (directory, namespace change, no room, I/O error): the caller takes
/// a checkpoint. Call without the filesystem lock, in task context.
pub(crate) fn fsync_rf(port: u32, ino: u32) -> Option<Result<(), ()>> {
    if !rf_enabled() { return None; }
    let _cp = CP_MUTEX.lock();
    struct P1 { dev: usize, runs: Vec<(u64, Vec<u8>)>, rec: Vec<u8>, at: u64, era: u64, pos: u32 }
    let p1 = {
        let mut mounts = lock_mounts();
        let ms = mounts.iter_mut().flatten().find(|m| m.port == port)?;
        if ms.read_only { return Some(Ok(())); }
        settle(ms);
        if ms.dur.rf_end == 0 { rf_full_note(); return None; }
        let done = cp_seq(port).map_or(0, |s| s.done.load(atomic::Ordering::SeqCst));
        let dentry = match ms.dur.ns.get(&ino) {
            Some(&(seq, created)) if seq > done => { if created { true } else { return None; } }
            _ => false,
        };
        let ia = nat_lookup(ms, ino);
        if ia == 0 { return None; }
        let (mode, links, pino) = { let b = ms.cache.read(ms.dev, ia as u64); (inode_mode(b), inode_links(b), r32(b, 84)) };
        if mode & S_IFMT != S_IFREG || links == 0 { return None; }
        if dentry {
            if links != 1 || pino == 0 { return None; }
            if let Some(&(seq, _)) = ms.dur.ns.get(&pino) { if seq > done { return None; } }
            if nat_lookup(ms, pino) == 0 { return None; }
        }
        // The file's dirty blocks: data tagged with its inode, node blocks
        // whose footer names it (and whose NAT entry points at them).
        let main = ms.sb.main_blkaddr as u64;
        let mut data_idx: Vec<usize> = Vec::new();
        let mut node_idx: Vec<(usize, u32)> = Vec::new();
        for i in 0..ms.cache.meta.len() {
            let m = ms.cache.meta[i];
            if !m.dirty || m.blk_no < main || m.blk_no == NULL_BLK { continue; }
            if m.udata {
                if m.owner == ino && sit_valid(ms, m.blk_no) { data_idx.push(i); }
                continue;
            }
            let (nid, own) = { let d = ms.cache.data(i); (r32(d, NODE_FOOTER_OFF), r32(d, NODE_FOOTER_OFF + 4)) };
            if own == ino && nid != 0 && nat_lookup(ms, nid) as u64 == m.blk_no { node_idx.push((i, nid)); }
        }
        if node_idx.is_empty() && data_idx.is_empty() && !dentry { return Some(Ok(())); }
        if dentry && node_idx.is_empty() { return None; }
        if node_idx.len() > RF_MAX_NODES { return None; }
        let need = if node_idx.is_empty() { 0 } else { 1 + node_idx.len() as u32 };
        if ms.dur.rf_next + need > ms.dur.rf_end { rf_full_note(); return None; }
        // The record.
        let mut rec: Vec<u8> = Vec::new();
        if rec.try_reserve_exact(need as usize * BLOCK_SIZE).is_err() { return None; }
        if need > 0 {
            rec.resize(BLOCK_SIZE, 0);
            w32(&mut rec, 0, RF_MAGIC);
            w32(&mut rec, 4, if dentry { RF_FLAG_DENTRY } else { 0 });
            w64(&mut rec, 8, ms.dur.durable_ver);
            w32(&mut rec, 16, ms.dur.rf_seq);
            w32(&mut rec, 20, node_idx.len() as u32);
            w32(&mut rec, 28, ino);
            w32(&mut rec, 32, pino);
            for (q, &(i, nid)) in node_idx.iter().enumerate() {
                w32(&mut rec, RF_HDR + 8 * q, nid);
                w32(&mut rec, RF_HDR + 8 * q + 4, ms.cache.meta[i].blk_no as u32);
            }
            for &(i, _) in &node_idx { rec.extend_from_slice(ms.cache.data(i)); }
            let crc = crc32_upd(crc32_upd(0, &rec[..BLOCK_SIZE]), &rec[BLOCK_SIZE..]);
            w32(&mut rec, 24, crc);
            // The same nodes as this file's last record, no new data: on
            // the medium already.
            let body = crc32_upd(0, &rec[RF_HDR..]);
            if data_idx.is_empty() && ms.dur.rf_last.get(&ino) == Some(&body) { return Some(Ok(())); }
            ms.dur.rf_last.insert(ino, body);
        }
        data_idx.sort_unstable_by_key(|&i| ms.cache.meta[i].blk_no);
        let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut k = 0;
        while k < data_idx.len() {
            let mut n = 1;
            while k + n < data_idx.len() && n < MAX_RUN
                && ms.cache.meta[data_idx[k + n]].blk_no == ms.cache.meta[data_idx[k]].blk_no + n as u64 { n += 1; }
            let mut v: Vec<u8> = Vec::new();
            if v.try_reserve_exact(n * BLOCK_SIZE).is_err() { return None; }
            for q in 0..n { v.extend_from_slice(ms.cache.data(data_idx[k + q])); }
            runs.push((ms.cache.meta[data_idx[k]].blk_no, v));
            k += n;
        }
        for &i in &data_idx { ms.cache.set_flags(i, false, true); }
        let pos = ms.dur.rf_next;
        ms.dur.rf_next += need;
        if rf_high(ms) && !RF_KICK.swap(true, atomic::Ordering::Relaxed) {
            let n = RF_KICKS.fetch_add(1, atomic::Ordering::Relaxed);
            if n == 0 || n.is_power_of_two() {
                dbg_str("[F2FS] roll-forward area 75% used, background checkpoint requested (count ");
                dbg_dec((n + 1) as usize);
                dbg_str(")\n");
            }
        }
        if need > 0 { ms.dur.rf_seq += 1; }
        let at = pack_base(ms, ms.dur.durable_pack) + pos as u64;
        WB_INFLIGHT.store(true, atomic::Ordering::Release);
        P1 { dev: ms.dev, runs, rec, at, era: ms.dur.durable_ver, pos }
    };
    // Phase 2: data, flush, record, flush.
    let mut ok = true;
    let mut cmds = 0u32;
    for (start, data) in &p1.runs { ok &= dws(p1.dev, *start, data, &mut cmds); }
    if !p1.runs.is_empty() || EVICT_UNFLUSHED.swap(false, atomic::Ordering::AcqRel) { ok &= dflush(p1.dev); }
    if ok && !p1.rec.is_empty() {
        ok &= dws(p1.dev, p1.at, &p1.rec, &mut cmds);
        ok &= dflush(p1.dev);
    }
    WB_INFLIGHT.store(false, atomic::Ordering::Release);
    // Phase 3.
    let mut mounts = lock_mounts();
    if let Some(ms) = mounts.iter_mut().flatten().find(|m| m.port == port) {
        for (start, data) in &p1.runs {
            for q in 0..(data.len() / BLOCK_SIZE) as u64 {
                if let Some(i) = ms.cache.find(start + q) {
                    if ms.cache.meta[i].wb {
                        let d = ms.cache.meta[i].dirty || !ok;
                        ms.cache.set_flags(i, d, false);
                    }
                }
            }
        }
        if !ok && ms.dur.durable_ver == p1.era {
            // A torn record may lie at `pos`: no more records this era.
            ms.dur.rf_end = 0;
            let _ = p1.pos;
        }
    }
    drop(mounts);
    if ok { RF_COUNT.fetch_add(1, atomic::Ordering::Relaxed); Some(Ok(())) } else { None }
}

// ── Mount: roll-forward recovery ─────────────────────────────────────────────

/// One record read back at mount.
struct Rec { ino: u32, pino: u32, dentry: bool, nodes: Vec<(u32, u32)>, imgs: Vec<u8> }

/// Records of era `ver` in pack `base` from `start`; returns them and the
/// block after the last valid one.
fn read_records(dev: usize, base: u64, start: u32, end: u32, ver: u64) -> (Vec<Rec>, u32, u32) {
    let mut out = Vec::new();
    let mut pos = start;
    let mut seq = 0u32;
    let mut hdr = [0u8; BLOCK_SIZE];
    while start != 0 && pos + 1 <= end {
        if !virtio_blk::read_block(dev, base + pos as u64, &mut hdr) { break; }
        if r32(&hdr, 0) != RF_MAGIC || r64(&hdr, 8) != ver || r32(&hdr, 16) != seq { break; }
        let n = r32(&hdr, 20) as usize;
        if n == 0 || n > RF_MAX_NODES || pos + 1 + n as u32 > end { break; }
        let mut imgs = alloc::vec![0u8; n * BLOCK_SIZE];
        if !virtio_blk::read_blocks(dev, base + pos as u64 + 1, &mut imgs) { break; }
        let want = r32(&hdr, 24);
        let mut h0 = hdr;
        w32(&mut h0, 24, 0);
        if crc32_upd(crc32_upd(0, &h0), &imgs) != want { break; }
        let nodes = (0..n).map(|q| (r32(&hdr, RF_HDR + 8 * q), r32(&hdr, RF_HDR + 8 * q + 4))).collect();
        out.push(Rec { ino: r32(&hdr, 28), pino: r32(&hdr, 32), dentry: r32(&hdr, 4) & RF_FLAG_DENTRY != 0,
                       nodes, imgs });
        pos += 1 + n as u32;
        seq += 1;
    }
    (out, pos, seq)
}

fn sit_set(ms: &mut MountState, a: u32, used: bool) {
    if a < ms.sb.main_blkaddr { return; }
    if let Some((seg, b)) = blkaddr_to_seg(ms, a) {
        if seg >= ms.sb.seg_cnt_main { return; }
        if used { sit_mark_block_used(ms, seg, b); } else { sit_mark_block_free(ms, seg, b); }
    }
}

/// Roll the durable pack's records forward into the cache (called at mount,
/// after the journal replay, before the mount checkpoint). Returns how many
/// records were applied.
pub(crate) fn recover(ms: &mut MountState, rf_start: u32, tail: u32) -> u32 {
    let base = pack_base(ms, ms.dur.durable_pack);
    let end = pack_blocks(ms) - tail;
    let (recs, next, seq) = read_records(ms.dev, base, rf_start, end, ms.dur.durable_ver);
    ms.dur.rf_next = if rf_start == 0 { 0 } else { next };
    ms.dur.rf_end = if rf_start == 0 { 0 } else { end };
    ms.dur.rf_start = rf_start;
    ms.dur.rf_seq = seq;
    if recs.is_empty() { return 0; }
    let mut inos: Vec<u32> = Vec::new();
    for r in &recs { if !inos.contains(&r.ino) { inos.push(r.ino); } }
    // Three phases (lane f2fsrecov). Every record's blocks were allocated
    // after the checkpoint, so the checkpointed SIT still calls them free and
    // the logs' write pointers sit right before them. Restoring a name
    // (`recover_dentry` -> `dir_add_entry`) can allocate a dentry or node
    // block; done while a LATER file's records were not yet in the SIT, the
    // log handed out one of that file's blocks (on a near-full volume, the
    // very next one): two owners, a garbled directory block and lost names.
    // So: (1) put every record's node images and NAT entries in place,
    // (2) apply all SIT frees and then all SIT uses, (3) only then restore
    // names, which may allocate. Frees go before uses so a block one file
    // dropped and another took cannot end up free.
    let mut frees: Vec<u32> = Vec::new();
    let mut uses: Vec<u32> = Vec::new();
    let mut dentries: Vec<(u32, u32)> = Vec::new();
    for &ino in &inos {
        let (old_n, old_d) = tree(ms, ino);
        let mut dentry = None;
        for r in recs.iter().filter(|r| r.ino == ino) {
            for (q, &(nid, addr)) in r.nodes.iter().enumerate() {
                let img: &[u8; BLOCK_SIZE] = r.imgs[q * BLOCK_SIZE..(q + 1) * BLOCK_SIZE].try_into().unwrap();
                if addr < ms.sb.main_blkaddr || nid == 0 { continue; }
                ms.cache.write(ms.dev, addr as u64, img);
                nat_update(ms, nid, addr);
                if nid >= ms.cp.next_free_nid { ms.cp.next_free_nid = nid + 1; }
            }
            if r.dentry { dentry = Some(r.pino); }
        }
        let (new_n, new_d) = tree(ms, ino);
        let old_a: BTreeSet<u32> = old_n.iter().map(|x| x.1).chain(old_d.iter().copied()).collect();
        let new_a: BTreeSet<u32> = new_n.iter().map(|x| x.1).chain(new_d.iter().copied()).collect();
        frees.extend(old_a.difference(&new_a).copied());
        uses.extend(new_a.difference(&old_a).copied());
        let new_ids: BTreeSet<u32> = new_n.iter().map(|x| x.0).collect();
        for &(nid, _) in &old_n { if !new_ids.contains(&nid) { nat_clear(ms, nid); } }
        if let Some(p) = dentry { dentries.push((ino, p)); }
    }
    for &a in &frees { sit_set(ms, a, false); }
    for &a in &uses { sit_set(ms, a, true); }
    for &(ino, p) in &dentries { recover_dentry(ms, ino, p); }
    dbg_str("[F2FS] roll-forward: ");
    dbg_dec(recs.len());
    dbg_str(" fsync record(s) of ");
    dbg_dec(inos.len());
    dbg_str(" file(s) recovered (area ");
    dbg_dec(rf_start as usize);
    dbg_str("..");
    dbg_dec(next as usize);
    dbg_str(" of ");
    dbg_dec(end as usize);
    dbg_str(")\n");
    recs.len() as u32
}

/// A file created after the checkpoint: put its name back in its parent.
fn recover_dentry(ms: &mut MountState, ino: u32, pino: u32) {
    if let Err(why) = recover_dentry_in(ms, ino, pino) {
        dbg_str("[F2FS] roll-forward: name of ino ");
        dbg_dec(ino as usize);
        dbg_str(" NOT restored: ");
        dbg_str(why);
        dbg_str("\n");
    }
}

fn recover_dentry_in(ms: &mut MountState, ino: u32, pino: u32) -> Result<(), &'static str> {
    let pa = nat_lookup(ms, pino);
    if pa == 0 { return Err("parent gone"); }
    if !{ let b = ms.cache.read(ms.dev, pa as u64); inode_is_dir(b) } { return Err("parent not a dir"); }
    let ia = nat_lookup(ms, ino);
    let ib = read_block_copy(ms, ia);
    let nl = (r32(&ib, INO_NAMELEN) as usize).min(NAME_MAX);
    if nl == 0 { return Err("no name"); }
    let name: Vec<u8> = ib[INO_NAME..INO_NAME + nl].to_vec();
    let cur = dir_lookup(ms, pino, &name);
    if cur == ino { return Ok(()); }
    if cur != 0 {
        // The name pointed at an older file whose removal was not durable.
        let ca = nat_lookup(ms, cur);
        let cb = read_block_copy(ms, ca);
        if inode_is_dir(&cb) { return Err("name taken by a dir"); }
        dir_remove_entry(ms, pino, &name);
        let links = inode_links(&cb);
        if links > 1 {
            let b = ms.cache.get_mut(ms.dev, ca as u64);
            w32(b, INO_LINKS, links - 1);
        } else {
            free_inode_data_and_nodes(ms, cur);
            free_block(ms, ca);
            node_freed(ms, cur);
        }
    }
    if !dir_add_entry(ms, pino, &name, ino, DT_REG) { return Err("dir_add_entry failed"); }
    Ok(())
}

/// The emergency checkpoint (out of memory for the snapshot, or no free
/// segments for a journal that does not fit its pack). Lock held and no other
/// writer. Same journal/commit layout as `cp_prepare`/`cp_write`, but zero-copy:
/// blocks go to the device straight from the cache and the index lists live in
/// arrays preallocated at mount. It NEVER writes a metadata block in place
/// before the commit: if the journal cannot be placed (or a write fails) it
/// writes nothing it cannot take back, keeps every block dirty and returns false.
pub(crate) fn cp_emergency(ms: &mut MountState, umount: bool) -> bool {
    static COUNT: atomic::AtomicU64 = atomic::AtomicU64::new(0);
    let n = COUNT.fetch_add(1, atomic::Ordering::Relaxed);
    if n == 0 || n.is_power_of_two() {
        dbg_str("[F2FS] checkpoint: emergency (zero-copy) commit, count ");
        dbg_dec((n + 1) as usize);
        dbg_str("\n");
    }
    settle(ms);
    let mut a = core::mem::take(&mut ms.dur.emerg_a);
    let mut b = core::mem::take(&mut ms.dur.emerg_b);
    let ok = cp_emergency_in(ms, umount, &mut a, &mut b);
    ms.dur.emerg_a = a;
    ms.dur.emerg_b = b;
    if !ok { ms.dirty_writes = ms.dirty_writes.max(1); }
    ok
}

fn cp_emergency_in(ms: &mut MountState, umount: bool, a: &mut Vec<u32>, b: &mut Vec<u32>) -> bool {
    a.clear();
    b.clear();
    let dev = ms.dev;
    let main = ms.sb.main_blkaddr as u64;
    let cp_lo = ms.sb.cp_blkaddr as u64;
    let cp_hi = cp_lo + 2 * pack_blocks(ms) as u64;
    // Every dirty or in-write-back slot, in block order.
    for i in 0..ms.cache.meta.len() {
        let m = ms.cache.meta[i];
        if !(m.dirty || m.wb) { continue; }
        if a.len() == a.capacity() {
            dbg_str("[F2FS] checkpoint refused: more dirty blocks than the emergency index arrays hold\n");
            return false;
        }
        a.push(i as u32);
    }
    a.sort_unstable_by_key(|&i| ms.cache.meta[i as usize].blk_no);
    // Classify exactly like cp_prepare: direct (kept in `a`, compacted) and
    // journaled (`b`); blocks the checkpoint marks free are dead.
    let mut nd = 0;
    for q in 0..a.len() {
        let i = a[q] as usize;
        let m = ms.cache.meta[i];
        if m.blk_no >= cp_lo && m.blk_no < cp_hi { continue; }
        if m.blk_no >= main && !sit_valid(ms, m.blk_no) { ms.cache.set_flags(i, false, false); continue; }
        if m.blk_no >= main && (m.udata || is_fresh(ms, m.blk_no)) {
            a[nd] = i as u32;
            nd += 1;
        } else {
            if b.len() == b.capacity() { return false; }
            b.push(i as u32);
        }
    }
    a.truncate(nd);
    let j = b.len();
    let h = (j + JIX_PER_BLK - 1) / JIX_PER_BLK;
    let total = (h + j) as u32;
    let pack_cap = pack_room(ms) - 1;
    let ext_n = if total > pack_cap { ((total - pack_cap) as usize + 511) / 512 } else { 0 };
    let mut ext_arr = [0u32; CPJ_EXT_MAX];
    if ext_n > CPJ_EXT_MAX || (ext_n > 0 && !pick_ext(ms, &mut ext_arr, ext_n)) {
        dbg_str("[F2FS] checkpoint refused: no journal space; volume left at the previous checkpoint\n");
        return false;
    }
    let ext_segs = &ext_arr[..ext_n];
    let logs = crate::linux_logs(ms, ext_segs);
    let mut ext = [0u64; CPJ_EXT_MAX];
    for (q, &sg) in ext_segs.iter().enumerate() {
        ext[q] = main + sg as u64 * ms.sb.blocks_per_seg as u64;
    }
    // Commit block and the snapshot bookkeeping, in cp_prepare's order.
    let pack = 1 - ms.dur.durable_pack;
    let (cp_blkno, mut cp_buf) = build_cp_block(ms, umount, pack, &logs);
    w32(&mut cp_buf, CPJ_MAGIC_OFF, CPJ_MAGIC);
    w32(&mut cp_buf, CPJ_INCOMPAT_OFF, if rf_enabled() { CPJ_INCOMPAT_RF } else { 0 });
    put_orphans(ms, &mut cp_buf);
    w32(&mut cp_buf, CPJ_BLOCKS_OFF, total);
    w32(&mut cp_buf, CPJ_COUNT_OFF, j as u32);
    w32(&mut cp_buf, CPJ_NEXT_OFF, ext_n as u32);
    for (q, &sg) in ext_segs.iter().enumerate() { w32(&mut cp_buf, CPJ_EXT_OFF + 4 * q, sg); }
    let rf_start = if ext_n == 0 && 1 + total < pack_room(ms) { 1 + total } else { 0 };
    prot_snapshot(ms);
    for &sg in ext_segs {
        let e = ms.prot_pending.entry(sg).or_insert([0; 8]);
        *e = [u64::MAX; 8];
    }
    ms.dur.fresh_inflight = core::mem::take(&mut ms.dur.fresh_pending);
    let pend = core::mem::take(&mut ms.dur.nid_pending);
    for n in pend {
        if nat_lookup(ms, n) == 0 { ms.dur.nid_inflight.push(n); }
    }
    let ver = ms.cp.ver;
    ms.dur.inflight_ver = ver;
    ms.dur.inflight_pack = pack;
    ms.dur.inflight_rf_start = rf_start;
    let pbase = pack_base(ms, pack);
    // Phase 1: direct blocks, then the journal (index blocks, then images).
    let mut ok = true;
    for &i in a.iter() {
        ok &= dw(dev, ms.cache.meta[i as usize].blk_no, ms.cache.data(i as usize));
    }
    let mut crc = 0u32;
    if j > 0 {
        let mut ix = [0u8; BLOCK_SIZE];
        let mut k = 0u32;
        for chunk in b.chunks(JIX_PER_BLK) {
            ix.fill(0);
            w32(&mut ix, 0, JIX_MAGIC);
            w32(&mut ix, 4, chunk.len() as u32);
            w64(&mut ix, 8, ver);
            for (q, &i) in chunk.iter().enumerate() {
                w32(&mut ix, JIX_HDR + 4 * q, ms.cache.meta[i as usize].blk_no as u32);
            }
            crc = crc32_upd(crc, &ix);
            ok &= dw(dev, jaddr_raw(pbase, pack_cap, &ext, k), &ix);
            k += 1;
        }
        for &i in b.iter() {
            let d = ms.cache.data(i as usize);
            crc = crc32_upd(crc, d);
            ok &= dw(dev, jaddr_raw(pbase, pack_cap, &ext, k), d);
            k += 1;
        }
    }
    ok &= dflush(dev);
    if ok {
        // Phase 2: the commit block.
        w32(&mut cp_buf, CPJ_CRC_OFF, crc);
        cp_seal(&mut cp_buf);
        ok &= dw(dev, cp_blkno, &cp_buf);
        let zero = core::mem::replace(&mut ms.dur.tail_dirty[pack as usize], false);
        let mut cmds = 0;
        if !write_tail(dev, pbase + pack_room(ms) as u64, &cp_buf, zero, &mut cmds) {
            ms.dur.tail_dirty[pack as usize] = true;
        }
        ok &= dflush(dev);
    }
    if ok {
        if let Some(c) = committed_slot(ms.port) { c.fetch_max(ver, atomic::Ordering::SeqCst); }
        // Direct blocks are on the medium: clean. Journaled blocks go home.
        for &i in a.iter() { ms.cache.set_flags(i as usize, false, false); }
        let mut home_ok = true;
        for &i in b.iter() {
            home_ok &= dw(dev, ms.cache.meta[i as usize].blk_no, ms.cache.data(i as usize));
        }
        home_ok &= dflush(dev);
        if home_ok { for &i in b.iter() { ms.cache.set_flags(i as usize, false, false); } }
    }
    settle(ms);
    ok
}
