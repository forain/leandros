//! Runtime block-device registry and the 4096-byte block façade.
//!
//! Every disk — virtio-blk at boot, USB mass storage whenever it finishes
//! enumerating — is a `&'static dyn BlockDevice` registered here and gets an
//! index plus a Linux name. Partitions found on a disk become further entries
//! (`sda2`, `vda1`) that point at the same device with a sector offset.
//!
//! # Index and name stability
//!
//! * Indices are **append-only**: an entry, once registered, keeps its index
//!   for the life of the kernel and is never removed.
//! * At boot, every virtio disk is registered first, in PCI order, before any
//!   partition scan — so QEMU's vda/vdb/vdc stay at indices 0/1/2 exactly as
//!   before. Partitions of those disks (the vda boot image's ESP) are scanned
//!   only afterwards and land at index 3 and up.
//! * Names are `<class><letter>` with a per-class letter counter (`vda`,
//!   `vdb`, …; `sda`, `sdb`, …), and `<disk><N>` (or `<disk>p<N>` when the
//!   disk name ends in a digit) for partitions.
//!
//! # The façade
//!
//! `read_block`/`read_blocks`/`write_block`/`flush`/`has_f2fs`/`info`/
//! `device_count` keep the signatures every caller (servers/f2fs, the VFS
//! `DiskOps`, `sys_mount`) already used with the old compile-time
//! `virtio_blk` alias. Blocks are 4096 bytes; the façade converts to device
//! sectors, adds a partition's start, and refuses I/O past a partition's end.
//!
//! # Locking
//!
//! `REG` is held only to copy an entry out or append one. All I/O runs after
//! it is dropped, against the device's own lock, so a slow USB stick never
//! stalls virtio I/O and registration from a USB enumeration thread never
//! blocks readers.

use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

pub use crate::blockdev::{
    is_f2fs_block0, parse_root_arg, BlkErr, BlockDevice, BootDisk, DiskDesc, RootErr, NAME_MAX,
};
use crate::partscan::{self, PartType, PartUuid, Table};
use crate::pci::{serial_debug as log, serial_debug_dec as log_dec};

/// Registry capacity: disks plus their partitions.
pub const MAX_ENTRIES: usize = 64;
const BLOCK: usize = 4096;

#[derive(Clone, Copy)]
struct Entry {
    dev: &'static dyn BlockDevice,
    name: [u8; NAME_MAX],
    nlen: u8,
    /// `/dev/<name>`, leaked once at registration (entries are permanent).
    path: &'static str,
    parent: Option<u16>,
    pno: u32,
    /// First sector within `dev` (0 for a whole disk).
    start: u64,
    /// Length in sectors (0 = unknown, whole disks only).
    nsectors: u64,
    ss: u32,
    /// Starts on a 4096-byte boundary of the device: required for f2fs.
    aligned: bool,
    ptype: Option<PartType>,
    partuuid: Option<PartUuid>,
    /// Disk-level identity from its partition table (whole disks only).
    disk_id: Option<BootDisk>,
    class: &'static str,
    ordinal: u16,
    removable: bool,
}

impl Entry {
    fn name(&self) -> &[u8] { &self.name[..self.nlen as usize] }
}

struct Registry {
    e: [Option<Entry>; MAX_ENTRIES],
    n: usize,
}

static REG: Mutex<Registry> = Mutex::new(Registry { e: [None; MAX_ENTRIES], n: 0 });
/// Published entry count; an index below it always names a filled slot.
static COUNT: AtomicUsize = AtomicUsize::new(0);

fn get(idx: usize) -> Option<Entry> {
    if idx >= COUNT.load(Ordering::Acquire) { return None; }
    REG.lock().e.get(idx).copied().flatten()
}

fn leak_path(name: &[u8]) -> &'static str {
    let mut s = alloc::string::String::from("/dev/");
    s.push_str(core::str::from_utf8(name).unwrap_or("?"));
    alloc::boxed::Box::leak(s.into_boxed_str())
}

fn push_dec(out: &mut [u8; NAME_MAX], mut n: usize, v: u32) -> Option<usize> {
    let mut d = [0u8; 10];
    let mut i = 0;
    let mut x = v;
    loop { d[i] = b'0' + (x % 10) as u8; i += 1; x /= 10; if x == 0 { break; } }
    if n + i > NAME_MAX { return None; }
    while i > 0 { i -= 1; out[n] = d[i]; n += 1; }
    Some(n)
}

/// Append an entry under the lock, naming it there (so two concurrent
/// registrations of the same class cannot draw the same letter).
fn push(mut e: Entry) -> Result<usize, BlkErr> {
    let mut t = REG.lock();
    if t.n >= MAX_ENTRIES { return Err(BlkErr::Full); }
    let mut name = [0u8; NAME_MAX];
    let nlen = match e.parent {
        None => {
            let ord = t.e[..t.n].iter().flatten()
                .filter(|x| x.parent.is_none() && x.class == e.class).count();
            let c = e.class.as_bytes();
            let mut n = c.len().min(NAME_MAX - 2);
            name[..n].copy_from_slice(&c[..n]);
            // a..z, then aa..zz (Linux sd naming).
            if ord >= 26 {
                name[n] = b'a' + ((ord / 26 - 1) % 26) as u8;
                n += 1;
            }
            name[n] = b'a' + (ord % 26) as u8;
            n += 1;
            e.ordinal = ord as u16;
            n
        }
        Some(p) => {
            let parent = t.e[p as usize].ok_or(BlkErr::NoDevice)?;
            let pn = parent.name();
            let mut n = pn.len();
            name[..n].copy_from_slice(pn);
            if pn.last().map_or(false, |c| c.is_ascii_digit()) {
                if n + 1 >= NAME_MAX { return Err(BlkErr::Full); }
                name[n] = b'p';
                n += 1;
            }
            e.ordinal = parent.ordinal;
            push_dec(&mut name, n, e.pno).ok_or(BlkErr::Full)?
        }
    };
    e.name = name;
    e.nlen = nlen as u8;
    e.path = leak_path(&name[..nlen]);
    let idx = t.n;
    t.e[idx] = Some(e);
    t.n += 1;
    COUNT.store(t.n, Ordering::Release);
    Ok(idx)
}

fn add_disk(dev: &'static dyn BlockDevice) -> Result<usize, BlkErr> {
    let ss = dev.sector_size();
    if ss < 512 || ss > BLOCK as u32 || !ss.is_power_of_two() { return Err(BlkErr::Unsupported); }
    let idx = push(Entry {
        dev,
        name: [0; NAME_MAX],
        nlen: 0,
        path: "",
        parent: None,
        pno: 0,
        start: 0,
        nsectors: dev.capacity_sectors(),
        ss,
        aligned: true,
        ptype: None,
        partuuid: None,
        disk_id: None,
        class: dev.name_class(),
        ordinal: 0,
        removable: dev.removable(),
    })?;
    if let Some(e) = get(idx) {
        log("[BLK] ");
        log(e.path);
        log(": ");
        log_dec(e.nsectors);
        log(" x ");
        log_dec(ss as u64);
        log(if e.removable { "-byte sectors, removable\n" } else { "-byte sectors\n" });
    }
    Ok(idx)
}

/// Read the partition table of whole disk `idx` and append its partitions.
fn scan_partitions(idx: usize) {
    let e = match get(idx) { Some(e) if e.parent.is_none() => e, _ => return };
    let dev = e.dev;
    let res = partscan::scan(e.ss, e.nsectors, |lba, buf| dev.read_sectors(lba, buf).is_ok());
    let disk_id = match res.table {
        Table::None => return,
        Table::Mbr { disk_sig } => BootDisk::Mbr(disk_sig),
        Table::Gpt { disk_guid, backup } => {
            if backup {
                log("[BLK] ");
                log(e.path);
                log(": primary GPT invalid, using backup header\n");
            }
            BootDisk::Gpt(disk_guid)
        }
    };
    if let Some(Some(slot)) = REG.lock().e.get_mut(idx) { slot.disk_id = Some(disk_id); }
    for p in res.parts {
        let aligned = p.aligned_4k(e.ss);
        let r = push(Entry {
            dev,
            name: [0; NAME_MAX],
            nlen: 0,
            path: "",
            parent: Some(idx as u16),
            pno: p.pno,
            start: p.start_lba,
            nsectors: p.num_lbas,
            ss: e.ss,
            aligned,
            ptype: Some(p.ptype),
            partuuid: Some(p.uuid),
            disk_id: None,
            class: e.class,
            ordinal: 0,
            removable: e.removable,
        });
        match r {
            Ok(pi) => if let Some(pe) = get(pi) {
                log("[BLK] ");
                log(pe.path);
                log(": start ");
                log_dec(p.start_lba);
                log(", ");
                log_dec(p.num_lbas);
                log(" sectors");
                if p.ptype.is_linux_fs() { log(", linux"); }
                if !aligned { log(", NOT 4K-aligned (no f2fs)"); }
                log("\n");
            },
            Err(_) => { log("[BLK] registry full, partition dropped\n"); break; }
        }
    }
}

// ── Registration API (boot + runtime) ────────────────────────────────────────

/// Register a whole disk and every partition on it; returns the disk's index.
///
/// Callable at any time after `init()` from thread context (it performs I/O:
/// sector 0, the GPT headers and the GPT entry array). The device must live
/// forever — a hot-plug driver typically `Box::leak`s its device object. The
/// returned index and the indices of its partitions are permanent; partitions
/// follow the disk at the next indices, in table order, unless a concurrent
/// registration interleaves (names and `parent` links stay correct either
/// way). Errors: `Unsupported` sector size, or `Full` registry.
pub fn register(dev: &'static dyn BlockDevice) -> Result<usize, BlkErr> {
    let idx = add_disk(dev)?;
    scan_partitions(idx);
    Ok(idx)
}

/// Probe the boot-time disks: every virtio-blk device first (indices 0..n in
/// PCI order), then their partition tables.
pub fn init() {
    let disks = crate::virtio_blk::probe_all();
    let n = disks.len();
    let mut first = usize::MAX;
    for d in disks {
        match add_disk(d) {
            Ok(i) => if first == usize::MAX { first = i; },
            Err(_) => log("[BLK] could not register a virtio disk\n"),
        }
    }
    if first != usize::MAX {
        for i in first..first + n { scan_partitions(i); }
    }
}

// ── 4096-byte block façade ───────────────────────────────────────────────────

/// Absolute device LBA of 4K block `blk` for a `len`-byte transfer, or None
/// when it would leave the entry.
fn locate(e: &Entry, blk: u64, len: usize) -> Option<u64> {
    let spb = (BLOCK / e.ss as usize) as u64;
    let lba = blk.checked_mul(spb)?;
    let n = (len / e.ss as usize) as u64;
    if e.nsectors != 0 && lba.checked_add(n)? > e.nsectors { return None; }
    e.start.checked_add(lba)
}

pub fn device_count() -> usize { COUNT.load(Ordering::Acquire) }

/// Read one 4096-byte block from device `dev_idx` at logical block `blk`.
pub fn read_block(dev_idx: usize, blk: u64, buf: &mut [u8; BLOCK]) -> bool {
    read_blocks(dev_idx, blk, buf)
}

/// Read `buf.len() / 4096` contiguous blocks starting at logical block `blk`.
/// `buf.len()` must be a non-zero multiple of 4096.
pub fn read_blocks(dev_idx: usize, blk: u64, buf: &mut [u8]) -> bool {
    if buf.is_empty() || buf.len() % BLOCK != 0 { return false; }
    let e = match get(dev_idx) { Some(e) => e, None => return false };
    match locate(&e, blk, buf.len()) {
        Some(lba) => e.dev.read_sectors(lba, buf).is_ok(),
        None => false,
    }
}

/// Write one 4096-byte block to device `dev_idx` at logical block `blk`.
pub fn write_block(dev_idx: usize, blk: u64, buf: &[u8; BLOCK]) -> bool {
    let e = match get(dev_idx) { Some(e) => e, None => return false };
    match locate(&e, blk, BLOCK) {
        Some(lba) => e.dev.write_sectors(lba, buf).is_ok(),
        None => false,
    }
}

/// Write `buf.len() / 4096` contiguous blocks starting at logical block `blk`
/// as one device request (`buf.len()` a non-zero multiple of 4096). Lets
/// f2fs write back a run of adjacent dirty blocks with one command instead
/// of one per block — on a USB stick each small write is expensive.
pub fn write_blocks(dev_idx: usize, blk: u64, buf: &[u8]) -> bool {
    if buf.is_empty() || buf.len() % BLOCK != 0 { return false; }
    let e = match get(dev_idx) { Some(e) => e, None => return false };
    match locate(&e, blk, buf.len()) {
        Some(lba) => e.dev.write_sectors(lba, buf).is_ok(),
        None => false,
    }
}

/// Commit `dev_idx`'s (or its disk's) volatile write cache.
pub fn flush(dev_idx: usize) -> bool {
    get(dev_idx).map_or(false, |e| e.dev.flush().is_ok())
}

/// True if entry `dev_idx` holds an f2fs volume (magic at byte 1024 of block
/// 0). Always false for a partition that does not start 4096-aligned.
pub fn has_f2fs(dev_idx: usize) -> bool {
    match get(dev_idx) { Some(e) if e.aligned => {}, _ => return false }
    // Heap, not stack: a 4 KiB frame inlined into a large caller has
    // overflowed this kernel's stack on AArch64 before.
    let mut buf = alloc::vec![0u8; BLOCK];
    read_blocks(dev_idx, 0, &mut buf) && is_f2fs_block0(&buf)
}

/// Metadata for `lsblk` (SYS_BLKDEV_INFO).
pub fn info(dev_idx: usize) -> Option<crate::BlkDevInfo> {
    let e = get(dev_idx)?;
    Some(crate::BlkDevInfo {
        total_blocks: e.nsectors * e.ss as u64 / BLOCK as u64,
        block_size: BLOCK as u32,
        fstype: if has_f2fs(dev_idx) { Some("f2fs") } else { None },
    })
}

// ── Raw log partition ────────────────────────────────────────────────────────

/// Registry index of the raw kernel-log partition (MBR type 0xDA or the
/// `LOG_GPT_GUID` type) on the disk the kernel booted from. Identified by
/// type, never by partition number. When the boot disk is unknown, a log
/// partition is accepted only if exactly one exists.
pub fn find_log_partition(boot: Option<BootDisk>) -> Option<usize> {
    let t = REG.lock();
    let mut found = None;
    let mut count = 0;
    for (i, e) in t.e[..t.n].iter().enumerate() {
        let e = match e { Some(e) => e, None => continue };
        match (e.parent, e.ptype) {
            (Some(p), Some(pt)) if pt.is_log() => {
                let pd = t.e[p as usize].and_then(|d| d.disk_id);
                match boot {
                    Some(b) => if pd == Some(b) { return Some(i); },
                    None => { count += 1; found = Some(i); }
                }
            }
            _ => {}
        }
    }
    if count == 1 { found } else { None }
}

/// Sector size and length in bytes of entry `idx`, for raw access.
pub fn raw_geometry(idx: usize) -> Option<(u32, u64)> {
    let e = get(idx)?;
    Some((e.ss, e.nsectors * e.ss as u64))
}

fn raw_locate(e: &Entry, byte_off: u64, len: usize) -> Option<u64> {
    let ss = e.ss as u64;
    if byte_off % ss != 0 || len == 0 || len as u64 % ss != 0 { return None; }
    let n = len as u64 / ss;
    let lba = byte_off / ss;
    if e.nsectors != 0 && lba.checked_add(n)? > e.nsectors { return None; }
    e.start.checked_add(lba)
}

/// Read `buf.len()` bytes at `byte_off` within entry `idx`; both must be
/// multiples of the sector size.
pub fn raw_read(idx: usize, byte_off: u64, buf: &mut [u8]) -> bool {
    let e = match get(idx) { Some(e) => e, None => return false };
    match raw_locate(&e, byte_off, buf.len()) {
        Some(lba) => e.dev.read_sectors(lba, buf).is_ok(),
        None => false,
    }
}

/// Write `buf` at `byte_off` within entry `idx`. `panic_path` selects the
/// device's `try_write_sectors` (fails fast instead of waiting for a lock)
/// and avoids the registry lock being waited on.
pub fn raw_write(idx: usize, byte_off: u64, buf: &[u8], panic_path: bool) -> bool {
    let e = if panic_path {
        // The registry lock may be held by the panicking CPU.
        let t = match REG.try_lock() { Some(t) => t, None => return false };
        match t.e.get(idx).copied().flatten() { Some(e) => e, None => return false }
    } else {
        match get(idx) { Some(e) => e, None => return false }
    };
    match raw_locate(&e, byte_off, buf.len()) {
        Some(lba) => if panic_path {
            e.dev.try_write_sectors(lba, buf).is_ok()
        } else {
            e.dev.write_sectors(lba, buf).is_ok()
        },
        None => false,
    }
}

/// Commit the device's write cache (best effort).
pub fn raw_flush(idx: usize) -> bool { flush(idx) }

// ── Names ────────────────────────────────────────────────────────────────────

/// Registry index of `name` (`"vdb"`, `"sda2"`; no `/dev/` prefix).
pub fn lookup_name(name: &str) -> Option<usize> {
    let t = REG.lock();
    t.e[..t.n].iter().position(|e| e.map_or(false, |e| e.name() == name.as_bytes()))
}

/// `/dev/<name>` of entry `idx` (`"/dev/?"` when there is none).
pub fn dev_path(idx: usize) -> &'static str {
    get(idx).map_or("/dev/?", |e| e.path)
}

/// Name, parent and geometry of entry `idx`, without any I/O.
pub fn describe(idx: usize) -> Option<DiskDesc> {
    let e = get(idx)?;
    Some(DiskDesc {
        name: e.name,
        nlen: e.nlen,
        parent: e.parent.map(|p| p as usize),
        pno: e.pno,
        start_bytes: e.start * e.ss as u64,
        bytes: e.nsectors * e.ss as u64,
        sector_size: e.ss,
        removable: e.removable,
        class: e.class,
        ordinal: e.ordinal,
    })
}

// ── /dev/root ────────────────────────────────────────────────────────────────

/// Resolve `/dev/root`, in order:
///
/// 1. `root=` on the kernel command line: `/dev/<name>` or `PARTUUID=<id>`.
///    An explicit `root=` that matches nothing is `NotFound` (the disk may
///    still be enumerating), never a silent fall-through to a guess.
/// 2. Legacy: index 1 when it is a whole virtio disk with f2fs magic — QEMU's
///    vdb, so the default boot is unchanged.
/// 3. The unique removable or `sd*` disk/partition with f2fs magic. Ties are
///    broken first by the boot disk (`boot`, from Limine), then by partition
///    type (MBR 0x83 / GPT Linux filesystem GUID); a remaining tie is
///    `Ambiguous`.
pub fn resolve_root(cmdline: Option<&str>, boot: Option<BootDisk>) -> Result<usize, RootErr> {
    if let Some(spec) = cmdline.and_then(parse_root_arg) {
        if let Some(name) = spec.strip_prefix("/dev/") {
            return lookup_name(name).ok_or(RootErr::NotFound);
        }
        if let Some(id) = spec.strip_prefix("PARTUUID=") {
            let t = REG.lock();
            return t.e[..t.n].iter()
                .position(|e| e.and_then(|e| e.partuuid).map_or(false, |u| u.matches(id)))
                .ok_or(RootErr::NotFound);
        }
        log("[BLK] root=");
        log(spec);
        log(" not understood (want /dev/<name> or PARTUUID=), auto-detecting\n");
    }

    if let Some(e) = get(1) {
        if e.parent.is_none() && e.class == "vd" && has_f2fs(1) { return Ok(1); }
    }

    // Snapshot, then probe for magic with the lock dropped.
    let n = device_count();
    let mut cands: alloc::vec::Vec<(usize, Entry)> = alloc::vec::Vec::new();
    for i in 0..n {
        if let Some(e) = get(i) {
            if (e.removable || e.class == "sd") && has_f2fs(i) { cands.push((i, e)); }
        }
    }
    if let Some(bd) = boot {
        let on_boot: alloc::vec::Vec<(usize, Entry)> = cands.iter().copied().filter(|(_, e)| {
            let disk = match e.parent { Some(p) => get(p as usize), None => Some(*e) };
            disk.and_then(|d| d.disk_id) == Some(bd)
        }).collect();
        if !on_boot.is_empty() { cands = on_boot; }
    }
    let linux: alloc::vec::Vec<usize> = cands.iter()
        .filter(|(_, e)| e.ptype.map_or(false, |t| t.is_linux_fs()))
        .map(|(i, _)| *i).collect();
    let pick = match (linux.len(), cands.len()) {
        (1, _) => Ok(linux[0]),
        (0, 1) => Ok(cands[0].0),
        (_, 0) => Err(RootErr::NotFound),
        _ => Err(RootErr::Ambiguous),
    };
    if pick == Err(RootErr::Ambiguous) {
        log("[BLK] /dev/root is ambiguous between:");
        for (_, e) in &cands { log(" "); log(e.path); }
        log(" (pass root= on the kernel command line)\n");
    }
    pick
}
