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

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::Mutex;

pub use crate::blockdev::{
    is_f2fs_block0, parse_root_arg, BlkErr, BlockDevice, BootDisk, DiskDesc, RootErr, NAME_MAX,
};
use crate::partscan::{self, PartType, PartUuid, Table};
use crate::pci::{serial_debug as log, serial_debug_dec as log_dec};

/// Registry capacity: disks plus their partitions.
pub const MAX_ENTRIES: usize = 128;
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
    /// The device was unplugged (lane usbunplug): the index stays taken
    /// (open files keep naming it, and fail with EIO), but it no longer has
    /// a name, I/O or geometry.
    gone: bool,
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
/// Serializes whole registrations (a disk plus its partition scan). `REG`
/// alone only makes each `push` atomic: two disks registering at once (an
/// NVMe namespace and a USB stick, each from its own probe thread) used to
/// interleave, so a disk's partitions did not follow it at consecutive
/// indices, and `init` scanned `first..first + n` assuming they did. A
/// sleeping lock, because a partition scan reads the disk.
static REGISTER: sched::ksync::KMutex<()> = sched::ksync::KMutex::new(());

fn get(idx: usize) -> Option<Entry> {
    get_any(idx).filter(|e| !e.gone)
}

/// [`get`], including entries whose device was unplugged.
fn get_any(idx: usize) -> Option<Entry> {
    if idx >= COUNT.load(Ordering::Acquire) { return None; }
    REG.lock().e.get(idx).copied().flatten()
}

/// Bumped whenever an entry goes away (lane usbunplug): the VFS mirror
/// rescans when it changes.
static REMOVED_GEN: AtomicUsize = AtomicUsize::new(0);

pub fn removed_generation() -> usize { REMOVED_GEN.load(Ordering::Acquire) }

/// Retire disk `idx` and its partitions after the device was unplugged.
/// Indices are never reused, so an open file that still names one of them
/// fails every request (EIO) instead of reaching a different disk. The
/// device object itself stays alive (drivers leak it). Returns the number
/// of entries retired.
pub fn remove_disk(idx: usize) -> usize {
    let _g = REGISTER.lock();
    let mut t = REG.lock();
    let n = t.n;
    let mut k = 0;
    for (i, slot) in t.e[..n].iter_mut().enumerate() {
        if let Some(e) = slot {
            if !e.gone && (i == idx || e.parent.map(|p| p as usize) == Some(idx)) {
                e.gone = true;
                k += 1;
            }
        }
    }
    drop(t);
    REMOVED_GEN.fetch_add(1, Ordering::AcqRel);
    k
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
            // Lowest ordinal no live disk of the class holds: a stick that
            // is unplugged and plugged back in gets its old name again.
            let taken = |o: usize| t.e[..t.n].iter().flatten()
                .any(|x| !x.gone && x.parent.is_none() && x.class == e.class && x.ordinal as usize == o);
            let ord = (0..).find(|&o| !taken(o)).unwrap_or(0);
            e.ordinal = ord as u16;
            if let Some(fixed) = e.dev.disk_name() {
                // The driver names the disk itself (`nvme0n1`).
                let f = fixed.as_bytes();
                if f.is_empty() || f.len() > NAME_MAX - 3 { return Err(BlkErr::Unsupported); }
                if t.e[..t.n].iter().flatten().any(|x| !x.gone && x.name() == f) { return Err(BlkErr::Unsupported); }
                name[..f.len()].copy_from_slice(f);
                f.len()
            } else {
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
                n
            }
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
        gone: false,
    })?;
    if let Some(e) = get(idx) {
        log("[BLK] ");
        log(e.path);
        log(": ");
        log_dec(e.nsectors);
        log(" x ");
        log_dec(ss as u64);
        log(if e.removable { "-byte sectors, removable" } else { "-byte sectors" });
        log(if dev.read_only() { ", read-only\n" } else { "\n" });
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
            gone: false,
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
/// follow the disk at the next indices, in table order: registrations are
/// serialized, so a concurrent one waits instead of interleaving. Errors:
/// `Unsupported` sector size, or `Full` registry.
pub fn register(dev: &'static dyn BlockDevice) -> Result<usize, BlkErr> {
    let _g = REGISTER.lock();
    let idx = add_disk(dev)?;
    scan_partitions(idx);
    Ok(idx)
}

/// Probe the boot-time disks: every virtio-blk device first (indices 0..n in
/// PCI order), then their partition tables.
pub fn init() {
    let disks = crate::virtio_blk::probe_all();
    let _g = REGISTER.lock();
    let mut added: alloc::vec::Vec<usize> = alloc::vec::Vec::with_capacity(disks.len());
    for d in disks {
        match add_disk(d) {
            Ok(i) => added.push(i),
            Err(_) => log("[BLK] could not register a virtio disk\n"),
        }
    }
    // Exactly the disks added above (a disk that failed to register leaves
    // no index behind, so `first..first + n` could name a partition).
    for i in added { scan_partitions(i); }
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
    let _io = io_enter();
    let e = match get(dev_idx) { Some(e) => e, None => return false };
    // Per-thread device-read count: tells a major page fault from a
    // page-cache hit (sched::page_fault).
    sched::note_block_read();
    match locate(&e, blk, buf.len()) {
        Some(lba) => e.dev.read_sectors(lba, buf).is_ok(),
        None => false,
    }
}

/// True when entry `dev_idx` (or the disk it is a partition of) refuses
/// writes. Every write entry point below checks it before the device sees
/// the request.
pub fn read_only(dev_idx: usize) -> bool {
    get(dev_idx).map_or(false, |e| e.dev.read_only())
}

/// Write one 4096-byte block to device `dev_idx` at logical block `blk`.
pub fn write_block(dev_idx: usize, blk: u64, buf: &[u8; BLOCK]) -> bool {
    let _io = io_enter();
    let e = match get(dev_idx) { Some(e) => e, None => return false };
    if e.dev.read_only() { return false; }
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
    let _io = io_enter();
    let e = match get(dev_idx) { Some(e) => e, None => return false };
    if e.dev.read_only() { return false; }
    match locate(&e, blk, buf.len()) {
        Some(lba) => e.dev.write_sectors(lba, buf).is_ok(),
        None => false,
    }
}

/// Commit `dev_idx`'s (or its disk's) volatile write cache.
pub fn flush(dev_idx: usize) -> bool {
    let _io = io_enter();
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

/// The f2fs volume label of the persistent `/home` volume: QEMU's vdc image
/// (`scripts/mkfs-f2fs-home.py`) and a USB stick's home partition
/// (`scripts/write-usb.sh`) both carry it, and `/etc/fstab` mounts
/// `LABEL=leandros-home` on `/home`. Never a root candidate.
pub const HOME_LABEL: &str = "leandros-home";

/// True if entry `idx` holds an f2fs volume whose label (the superblock's
/// UTF-16LE `volume_name`, byte 124 of the superblock) is exactly `label`.
/// ASCII labels only, which is all mkfs.f2fs -l and our scripts write.
pub fn f2fs_label_is(idx: usize, label: &str) -> bool {
    const SB: usize = 1024;
    const NAME: usize = SB + 124;
    match get(idx) { Some(e) if e.aligned => {}, _ => return false }
    let l = label.as_bytes();
    if l.is_empty() || l.len() >= 512 { return false; }
    let mut buf = alloc::vec![0u8; BLOCK];
    if !read_blocks(idx, 0, &mut buf) || !is_f2fs_block0(&buf) { return false; }
    let u = |i: usize| u16::from_le_bytes([buf[NAME + 2 * i], buf[NAME + 2 * i + 1]]);
    l.iter().enumerate().all(|(i, &c)| u(i) == c as u16) && u(l.len()) == 0
}

/// The whole disk an entry lives on (itself for a whole disk).
fn disk_of(idx: usize) -> Option<usize> {
    get(idx).map(|e| e.parent.map_or(idx, |p| p as usize))
}

/// Resolve a mount source given by identity rather than name:
/// `LABEL=<f2fs label>` or `PARTUUID=<id>`. Several volumes with one label
/// are told apart by the disk: one on the root's disk wins, then one on the
/// boot disk; a remaining tie is `Ambiguous`. `None` for any other form.
pub fn resolve_source(spec: &str, cmdline: Option<&str>, boot: Option<BootDisk>)
    -> Option<Result<usize, RootErr>>
{
    if let Some(id) = spec.strip_prefix("PARTUUID=") {
        let t = REG.lock();
        return Some(t.e[..t.n].iter()
            .position(|e| e.and_then(|e| e.partuuid).map_or(false, |u| u.matches(id)))
            .ok_or(RootErr::NotFound));
    }
    let label = spec.strip_prefix("LABEL=")?;
    let mut cands: alloc::vec::Vec<usize> =
        (0..device_count()).filter(|&i| f2fs_label_is(i, label)).collect();
    if cands.len() > 1 {
        let root_disk = resolve_root(cmdline, boot).ok().and_then(disk_of);
        let on_root: alloc::vec::Vec<usize> =
            cands.iter().copied().filter(|&i| root_disk.is_some() && disk_of(i) == root_disk).collect();
        if !on_root.is_empty() {
            cands = on_root;
        } else if let Some(bd) = boot {
            let on_boot: alloc::vec::Vec<usize> = cands.iter().copied()
                .filter(|&i| disk_of(i).and_then(get).and_then(|d| d.disk_id) == Some(bd)).collect();
            if !on_boot.is_empty() { cands = on_boot; }
        }
    }
    Some(match cands.len() {
        0 => Err(RootErr::NotFound),
        1 => Ok(cands[0]),
        _ => {
            log("[BLK] LABEL=");
            log(label);
            log(" is ambiguous between:");
            for &i in &cands { log(" "); log(dev_path(i)); }
            log("\n");
            Err(RootErr::Ambiguous)
        }
    })
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
    let _io = io_enter();
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
    let _io = if panic_path { None } else { Some(io_enter()) };
    if e.dev.read_only() { return false; }
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

/// Reboot / power-off: commit the write cache of every writable whole disk
/// (a USB stick gets SYNCHRONIZE CACHE after the kernel log's final write;
/// Linux `sd_shutdown`). Read-only disks are skipped. Returns (flushed,
/// failed).
pub fn flush_all_disks() -> (usize, usize) {
    let (mut ok, mut bad) = (0, 0);
    for i in 0..device_count() {
        let e = match get(i) { Some(e) => e, None => continue };
        if e.parent.is_some() || e.dev.read_only() { continue; }
        if e.dev.flush().is_ok() { ok += 1; } else { bad += 1; }
    }
    (ok, bad)
}

// ── System sleep gate (kernel pm) ───────────────────────────────────────────

/// Set while the devices are quiesced for a suspend cycle: new requests wait
/// (bounded, so a stuck cycle cannot wedge the system: after `PM_WAIT_MAX_NS`
/// a request goes through anyway and is counted in `PM_LEAKED`).
static PM_FROZEN: AtomicBool = AtomicBool::new(false);
static PM_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
static PM_WAITED: AtomicUsize = AtomicUsize::new(0);
static PM_LEAKED: AtomicUsize = AtomicUsize::new(0);
const PM_WAIT_MAX_NS: u64 = 10_000_000_000;

struct IoGuard;
impl Drop for IoGuard {
    fn drop(&mut self) { PM_INFLIGHT.fetch_sub(1, Ordering::SeqCst); }
}

fn io_enter() -> IoGuard {
    PM_INFLIGHT.fetch_add(1, Ordering::SeqCst);
    if !PM_FROZEN.load(Ordering::SeqCst) { return IoGuard; }
    PM_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
    PM_WAITED.fetch_add(1, Ordering::Relaxed);
    let deadline = sched::monotonic_ns().saturating_add(PM_WAIT_MAX_NS);
    loop {
        if sched::irqctx::can_sleep() { sched::kthread::msleep(1); } else { sched::kthread::udelay(200); }
        PM_INFLIGHT.fetch_add(1, Ordering::SeqCst);
        if !PM_FROZEN.load(Ordering::SeqCst) { return IoGuard; }
        if sched::monotonic_ns() >= deadline { PM_LEAKED.fetch_add(1, Ordering::Relaxed); return IoGuard; }
        PM_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Suspend hook: stop new requests, wait up to `timeout_ms` for the ones in
/// flight (all requests are synchronous), then commit every disk's write
/// cache. On a drain timeout the gate is reopened and the hook fails.
pub fn pm_freeze(timeout_ms: u64) -> Result<alloc::string::String, alloc::string::String> {
    PM_WAITED.store(0, Ordering::Relaxed);
    PM_LEAKED.store(0, Ordering::Relaxed);
    PM_FROZEN.store(true, Ordering::SeqCst);
    let t = sched::monotonic_ns();
    let deadline = t.saturating_add(timeout_ms * 1_000_000);
    while PM_INFLIGHT.load(Ordering::SeqCst) != 0 {
        if sched::monotonic_ns() >= deadline {
            PM_FROZEN.store(false, Ordering::SeqCst);
            return Err(alloc::format!("{} request(s) still in flight after {} ms", PM_INFLIGHT.load(Ordering::SeqCst), timeout_ms));
        }
        sched::kthread::msleep(1);
    }
    let drained = (sched::monotonic_ns() - t) / 1000;
    // flush_all_disks goes straight to the devices, under the gate.
    let (ok, bad) = flush_all_disks();
    if bad != 0 {
        PM_FROZEN.store(false, Ordering::SeqCst);
        return Err(alloc::format!("write-cache flush failed on {} of {} disk(s)", bad, ok + bad));
    }
    Ok(alloc::format!("{} disk(s): drained in {} us, write caches flushed", ok, drained))
}

/// Resume hook: reopen the gate; reports how many requests waited.
pub fn pm_thaw() -> Result<alloc::string::String, alloc::string::String> {
    PM_FROZEN.store(false, Ordering::SeqCst);
    let (w, l) = (PM_WAITED.load(Ordering::Relaxed), PM_LEAKED.load(Ordering::Relaxed));
    if l != 0 { return Err(alloc::format!("{} request(s) waited, {} passed the gate on timeout", w, l)); }
    Ok(alloc::format!("gate open; {} request(s) waited", w))
}

// ── Names ────────────────────────────────────────────────────────────────────

/// Registry index of `name` (`"vdb"`, `"sda2"`; no `/dev/` prefix).
pub fn lookup_name(name: &str) -> Option<usize> {
    let t = REG.lock();
    t.e[..t.n].iter().position(|e| e.map_or(false, |e| !e.gone && e.name() == name.as_bytes()))
}

/// `/dev/<name>` of entry `idx` (`"/dev/?"` when there is none).
pub fn dev_path(idx: usize) -> &'static str {
    get(idx).map_or("/dev/?", |e| e.path)
}

/// Name, parent and geometry of entry `idx`, without any I/O.
pub fn describe(idx: usize) -> Option<DiskDesc> {
    let e = get_any(idx)?;
    Some(DiskDesc {
        name: e.name,
        nlen: e.nlen,
        parent: e.parent.map(|p| p as usize),
        pno: e.pno,
        start_bytes: e.start * e.ss as u64,
        bytes: e.nsectors * e.ss as u64,
        sector_size: e.ss,
        removable: e.removable,
        read_only: e.dev.read_only(),
        class: e.class,
        ordinal: e.ordinal,
        gone: e.gone,
    })
}

// ── /dev/root ────────────────────────────────────────────────────────────────

static NO_HINT_WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// `root=LABEL=<f2fs label>`: the volume carrying that label. QEMU boot
/// images name their root this way (`leandros-root`), so extra virtio disks
/// cannot reorder it. A tie (e.g. a USB stick written from the same root
/// image) goes to the one whole virtio disk among them, else `Ambiguous`.
/// No match is `NotFound`: the disk may still be enumerating.
fn resolve_root_label(label: &str) -> Result<usize, RootErr> {
    let mut cands: alloc::vec::Vec<usize> =
        (0..device_count()).filter(|&i| f2fs_label_is(i, label)).collect();
    if cands.len() > 1 {
        let vd: alloc::vec::Vec<usize> = cands.iter().copied()
            .filter(|&i| get(i).map_or(false, |e| e.parent.is_none() && e.class == "vd")).collect();
        if vd.len() == 1 { cands = vd; }
    }
    match cands.len() {
        0 => Err(RootErr::NotFound),
        1 => Ok(cands[0]),
        _ => {
            log("[BLK] root=LABEL=");
            log(label);
            log(" is ambiguous between:");
            for &i in &cands { log(" "); log(dev_path(i)); }
            log("\n");
            Err(RootErr::Ambiguous)
        }
    }
}

/// Resolve `/dev/root`, in order:
///
/// 1. `root=` on the kernel command line: `/dev/<name>`, `PARTUUID=<id>` or
///    `LABEL=<f2fs label>` (what QEMU boot images pass).
///    An explicit `root=` that matches nothing is `NotFound` (the disk may
///    still be enumerating), never a silent fall-through to a guess.
/// 2. No hint (old images; logged once as a warning). Legacy: index 1 when it is a whole virtio disk with f2fs magic — QEMU's
///    vdb, so the default boot is unchanged.
/// 3. The unique removable or `sd*` disk/partition with f2fs magic that is not
///    the home volume (label `HOME_LABEL`). Ties are
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
        if let Some(label) = spec.strip_prefix("LABEL=") {
            return resolve_root_label(label);
        }
        log("[BLK] root=");
        log(spec);
        log(" not understood (want /dev/<name>, PARTUUID= or LABEL=), auto-detecting\n");
    } else if !NO_HINT_WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        log("[BLK] warning: no root= on the kernel command line; guessing /dev/root \
             from probe order (pass root=LABEL=, PARTUUID= or /dev/<name>)\n");
    }

    if let Some(e) = get(1) {
        if e.parent.is_none() && e.class == "vd" && has_f2fs(1) { return Ok(1); }
    }

    // Snapshot, then probe for magic with the lock dropped.
    let n = device_count();
    let mut cands: alloc::vec::Vec<(usize, Entry)> = alloc::vec::Vec::new();
    for i in 0..n {
        if let Some(e) = get(i) {
            // The home volume is f2fs too, on the same stick as the root:
            // it must never be taken for the root.
            if (e.removable || e.class == "sd") && has_f2fs(i) && !f2fs_label_is(i, HOME_LABEL) {
                cands.push((i, e));
            }
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
