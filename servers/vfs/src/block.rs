//! Block devices: `/dev/vd*`, partitions, loop devices, and the synthesized
//! `/sys/class/block` tree.
//!
//! # Why this lives inside `vfs-server`
//!
//! Everything a block device is, from userspace's point of view, is a VFS
//! question: a node in `/dev`, a subtree of `/sys`, an fd with a file offset,
//! and a handful of ioctls. The one thing that is *not* a VFS question is the
//! raw platter I/O, and `vfs-server` deliberately does not depend on the
//! `drivers` crate (see the `DMABUF_RELEASE` note in `lib.rs` — the VFS sits
//! below the device layer and an edge the other way would be a cycle in
//! spirit if not in cargo). So the disk backend arrives as a `&'static
//! DiskOps` vtable that `kernel/src/init.rs` registers once at boot, exactly
//! the shape the DRM dmabuf hook already uses. A null registration means "no
//! block backend on this build", and then every entry point here answers
//! ENXIO rather than inventing a device.
//!
//! # Why sysfs is synthesized on demand rather than stored
//!
//! The real kernel's sysfs is a view over live objects, not a filesystem with
//! contents. Materialising it here would mean N directory entries and ~8
//! attribute files per device in the tmpfs pool (a fixed 128-slot BSS array
//! whose slots real userland already competes for), *and* a coherence
//! problem: every BLKPG add/delete, every LOOP_SET_FD/LOOP_CLR_FD and every
//! `ftruncate` of a backing file would have to rewrite that tree, and any
//! path that forgot to would hand out a stale `size`. Synthesising means the
//! registry below is the single source of truth and a reader physically
//! cannot observe anything else: directories are `VnodeKind::SysBlock`
//! vnodes carrying only `(registry slot, level)`, and attribute files are
//! generated into an ephemeral pool slot at `open` time — the same mechanism
//! `/proc/meminfo` already uses (`gen_proc_system`).
//!
//! # Locking
//!
//! `BDEVS` and `LOOPS` are leaf locks: nothing is ever called *out* of this
//! module with either held. That matters because loop I/O re-enters the VFS
//! (`call_port` to the mount that owns the backing file), and `call_port`
//! runs the target server's handler synchronously on this very thread
//! (`ipc::port::send`). Every path here therefore copies what it needs out
//! from under the lock, drops it, and only then performs I/O.

use super::*;

/// Registry capacity: 3 virtio disks + 8 loop devices + partitions, with room
/// for a GPT's worth of partitions on two disks at once.
pub const MAX_BDEV: usize = 48;
/// Loop devices created at boot. Linux's `max_loop` default is 8 and every
/// node exists whether or not a backing file is attached, which is precisely
/// the contract `LOOP_CTL_GET_FREE` + `open("/dev/loopN")` relies on.
pub const MAX_LOOPS: usize = 8;
/// Longest device name we store inline (`loop0p15` is 8).
pub const NAME_MAX: usize = 16;

/// Slot id naming `/dev/loop-control`, which is a *character* device with no
/// registry entry of its own.
pub const LOOP_CONTROL: u16 = 0xFFFE;
/// "not a device" — the `/sys`, `/sys/class` and `/sys/class/block`
/// container directories, which belong to no particular device.
pub const NO_DEV: u16 = 0xFFFF;

/// The 512-byte unit every `size`/`start` sysfs attribute is counted in.
/// Linux reports these in 512-byte sectors *regardless* of the device's
/// logical block size, and disks-rs multiplies straight back by 512
/// (`disks::SECTOR_SIZE`), so anything else here scales every size wrongly.
pub const SECTOR: u64 = 512;

/// The block granularity of the disk backend (`drivers::blkdev` speaks 4096).
const DISK_BLOCK: usize = 4096;

// ── sysfs directory levels ───────────────────────────────────────────────────

pub const LVL_SYS: u8 = 0; // /sys
pub const LVL_CLASS: u8 = 1; // /sys/class
pub const LVL_BLOCK: u8 = 2; // /sys/class/block
pub const LVL_DEV: u8 = 3; // /sys/class/block/<name>
pub const LVL_QUEUE: u8 = 4; // /sys/class/block/<name>/queue
pub const LVL_DEVICE: u8 = 5; // /sys/class/block/<name>/device
pub const LVL_LOOP: u8 = 6; // /sys/class/block/<name>/loop

// ── Disk backend vtable ──────────────────────────────────────────────────────

/// The raw disk backend, registered once at boot from `kernel/src/init.rs`.
/// See the module docs for why this is a vtable and not a direct dependency.
pub struct DiskOps {
    /// Number of probed whole disks.
    pub disk_count: fn() -> usize,
    /// Capacity of disk `idx` in bytes; 0 when the device reported none.
    pub disk_bytes: fn(usize) -> u64,
    /// Read `buf.len() / 4096` contiguous 4096-byte blocks from block `blk`.
    /// `buf.len()` is always a non-zero multiple of 4096.
    pub read_blocks: fn(usize, u64, &mut [u8]) -> bool,
    /// Write exactly one 4096-byte block.
    pub write_block: fn(usize, u64, &[u8]) -> bool,
    /// Commit the device's volatile write cache.
    pub flush: fn(usize) -> bool,
}

static DISK_OPS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

fn ops() -> Option<&'static DiskOps> {
    let p = DISK_OPS.load(core::sync::atomic::Ordering::Acquire);
    if p == 0 { None } else { Some(unsafe { &*(p as *const DiskOps) }) }
}

// ── Registry ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Backing {
    /// Whole disk `idx` of the backend.
    Disk(u8),
    /// Loop device `n` (bound or not — the node always exists).
    Loop(u8),
    /// Partition `pno` of registry slot `parent`.
    Part { parent: u8, pno: u32 },
}

#[derive(Clone, Copy)]
struct BDev {
    in_use: bool,
    name: [u8; NAME_MAX],
    nlen: u8,
    backing: Backing,
    /// Byte offset of this device within its parent (0 for whole devices).
    start: u64,
    /// Length in bytes.
    size: u64,
    /// Logical (addressable) block size in bytes — 512 for everything we have.
    lbs: u32,
    major: u32,
    minor: u32,
}

impl BDev {
    const fn empty() -> Self {
        Self {
            in_use: false,
            name: [0; NAME_MAX],
            nlen: 0,
            backing: Backing::Disk(0),
            start: 0,
            size: 0,
            lbs: 512,
            major: 0,
            minor: 0,
        }
    }
    fn name(&self) -> &[u8] { &self.name[..self.nlen as usize] }
}

static BDEVS: Mutex<[BDev; MAX_BDEV]> =
    Mutex::new([const { BDev::empty() }; MAX_BDEV]);

/// One loop device's binding. The backing file is *our own* open — obtained
/// by re-opening the path the caller's fd names, straight against the owning
/// mount, never through an fd table. That is what makes the binding survive
/// the caller closing its fd, exactly as Linux's `loop` driver holds its own
/// `struct file` reference.
#[derive(Clone, Copy)]
struct LoopState {
    bound: bool,
    backing: VnodeKind,
    path: [u8; 128],
    plen: u8,
}

impl LoopState {
    const fn empty() -> Self {
        Self { bound: false, backing: VnodeKind::None, path: [0; 128], plen: 0 }
    }
}

static LOOPS: Mutex<[LoopState; MAX_LOOPS]> =
    Mutex::new([const { LoopState::empty() }; MAX_LOOPS]);

// ── Boot-time registration ───────────────────────────────────────────────────

/// Publish the disk backend and populate the registry with one node per
/// probed disk plus the eight always-present loop devices.
pub fn init(disk_ops: &'static DiskOps) {
    DISK_OPS.store(disk_ops as *const DiskOps as usize,
                   core::sync::atomic::Ordering::Release);

    // Size every disk BEFORE taking the registry lock: `disk_bytes` reaches
    // the driver, which busy-waits on a virtqueue round trip with its own
    // global lock held. Holding BDEVS across that would make the registry an
    // edge in the device-layer lock graph for no reason at all.
    let n = (disk_ops.disk_count)().min(26);
    let mut sizes = [0u64; 26];
    for i in 0..n { sizes[i] = (disk_ops.disk_bytes)(i); }

    let mut b = BDEVS.lock();
    for i in 0..n {
        let mut name = [0u8; NAME_MAX];
        name[0] = b'v';
        name[1] = b'd';
        name[2] = b'a' + i as u8;
        let slot = match b.iter().position(|e| !e.in_use) { Some(s) => s, None => break };
        b[slot] = BDev {
            in_use: true,
            name,
            nlen: 3,
            backing: Backing::Disk(i as u8),
            start: 0,
            size: sizes[i],
            lbs: 512,
            // 254 is what QEMU/Linux hands virtio-blk; 16 minors per disk
            // leaves room for the partition nodes BLKPG can add.
            major: 254,
            minor: (i * 16) as u32,
        };
    }
    for l in 0..MAX_LOOPS {
        let mut name = [0u8; NAME_MAX];
        name[..4].copy_from_slice(b"loop");
        name[4] = b'0' + l as u8;
        let slot = match b.iter().position(|e| !e.in_use) { Some(s) => s, None => break };
        b[slot] = BDev {
            in_use: true,
            name,
            nlen: 5,
            backing: Backing::Loop(l as u8),
            start: 0,
            size: 0, // unbound: Linux reports 0 sectors, and so do we
            lbs: 512,
            major: 7,
            minor: l as u32,
        };
    }
}

// ── Lookup ───────────────────────────────────────────────────────────────────

fn find_name(b: &[BDev; MAX_BDEV], name: &[u8]) -> Option<u16> {
    b.iter().position(|e| e.in_use && e.name() == name).map(|i| i as u16)
}

/// Resolve an absolute `/dev/...` path to a registry slot (or `LOOP_CONTROL`).
pub fn lookup_dev_node(path: &[u8]) -> Option<u16> {
    if path == b"/dev/loop-control" { return Some(LOOP_CONTROL); }
    let name = path.strip_prefix(b"/dev/".as_slice())?;
    if name.is_empty() || name.contains(&b'/') { return None; }
    find_name(&BDEVS.lock(), name)
}

/// Name of the `idx`-th registry entry, for enumerating `/dev`.
pub fn dev_node_name(idx: usize, out: &mut [u8; NAME_MAX]) -> Option<usize> {
    let b = BDEVS.lock();
    let e = b.iter().filter(|e| e.in_use).nth(idx)?;
    let n = e.nlen as usize;
    out[..n].copy_from_slice(e.name());
    Some(n)
}

/// Size in bytes of `dev` (0 for `/dev/loop-control` and unbound loops).
pub fn dev_size(dev: u16) -> u64 {
    if dev == LOOP_CONTROL || dev == NO_DEV { return 0; }
    let b = BDEVS.lock();
    b.get(dev as usize).filter(|e| e.in_use).map_or(0, |e| e.size)
}

/// `st_rdev` for the node, as a Linux `dev_t`.
pub fn dev_rdev(dev: u16) -> u64 {
    // /dev/loop-control is the misc device 10:237 on every Linux system, and
    // `fs::metadata` on it is part of the contract disks-rs exercises.
    if dev == LOOP_CONTROL { return makedev(10, 237); }
    let b = BDEVS.lock();
    b.get(dev as usize).filter(|e| e.in_use).map_or(0, |e| makedev(e.major, e.minor))
}

/// True when the node is a character device (`/dev/loop-control`); everything
/// else in the registry is a block device and must report `S_IFBLK`.
pub fn dev_is_char(dev: u16) -> bool { dev == LOOP_CONTROL }

/// A stable inode number for a block node, distinct from the pipe
/// (0x1000_0000), tmpfs (0x2000_0000) and pty ranges.
pub fn dev_ino(dev: u16) -> u64 { 0x4000_0000 + dev as u64 }

// ── Offset resolution ────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Root {
    Disk(u8),
    Loop(u8),
}

/// Collapse a registry slot to its backing root plus the absolute byte range
/// it occupies there. Partitions are never nested, so this walks at most one
/// level.
fn resolve(dev: u16) -> Option<(Root, u64, u64)> {
    let b = BDEVS.lock();
    let d = dev as usize;
    if d >= MAX_BDEV || !b[d].in_use { return None; }
    match b[d].backing {
        Backing::Disk(i) => Some((Root::Disk(i), 0, b[d].size)),
        Backing::Loop(n) => Some((Root::Loop(n), 0, b[d].size)),
        Backing::Part { parent, .. } => {
            let p = parent as usize;
            if p >= MAX_BDEV || !b[p].in_use { return None; }
            let root = match b[p].backing {
                Backing::Disk(i) => Root::Disk(i),
                Backing::Loop(n) => Root::Loop(n),
                // A partition of a partition cannot be created (BLKPG only
                // ever names a whole device), so this is unreachable.
                Backing::Part { .. } => return None,
            };
            Some((root, b[d].start, b[d].size))
        }
    }
}

// ── Raw disk I/O (4096-byte blocks, with read-modify-write bouncing) ─────────

fn disk_read(idx: usize, off: u64, buf: &mut [u8]) -> bool {
    let o = match ops() { Some(o) => o, None => return false };
    let mut done = 0usize;
    while done < buf.len() {
        let abs = off + done as u64;
        let blk = abs / DISK_BLOCK as u64;
        let within = (abs % DISK_BLOCK as u64) as usize;
        let left = buf.len() - done;
        if within == 0 && left >= DISK_BLOCK {
            let nblk = left / DISK_BLOCK;
            if !(o.read_blocks)(idx, blk, &mut buf[done..done + nblk * DISK_BLOCK]) {
                return false;
            }
            done += nblk * DISK_BLOCK;
        } else {
            // Unaligned head or short tail: bounce one block through the heap.
            // Never a stack array — a 4 KiB frame in a VFS call chain is how
            // this kernel overflows its guard-page-less stack.
            let mut tmp = alloc::vec![0u8; DISK_BLOCK];
            if !(o.read_blocks)(idx, blk, &mut tmp) { return false; }
            let n = (DISK_BLOCK - within).min(left);
            buf[done..done + n].copy_from_slice(&tmp[within..within + n]);
            done += n;
        }
    }
    true
}

fn disk_write(idx: usize, off: u64, buf: &[u8]) -> bool {
    let o = match ops() { Some(o) => o, None => return false };
    let mut done = 0usize;
    while done < buf.len() {
        let abs = off + done as u64;
        let blk = abs / DISK_BLOCK as u64;
        let within = (abs % DISK_BLOCK as u64) as usize;
        let left = buf.len() - done;
        let mut tmp = alloc::vec![0u8; DISK_BLOCK];
        let n = (DISK_BLOCK - within).min(left);
        if !(within == 0 && n == DISK_BLOCK) {
            // Partial block: read-modify-write, or the untouched bytes of the
            // block are zeroed out from under whoever owns them.
            if !(o.read_blocks)(idx, blk, &mut tmp) { return false; }
        }
        tmp[within..within + n].copy_from_slice(&buf[done..done + n]);
        if !(o.write_block)(idx, blk, &tmp) { return false; }
        done += n;
    }
    true
}

// ── Backing-file I/O (loop devices) ─────────────────────────────────────────

/// Read `buf.len()` bytes at `off` from a loop device's backing file. Holes
/// (a short read from a sparse file, or EOF) read back as zeros, which is
/// what a block device must do.
fn backing_read(kind: VnodeKind, off: u64, buf: &mut [u8]) -> bool {
    match kind {
        VnodeKind::MountedFile { port, file_id } => {
            let mut seek = Message::empty();
            seek.tag = VFS_LSEEK;
            seek.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            seek.data[8..16].copy_from_slice(&off.to_le_bytes());
            seek.data[16..24].copy_from_slice(&0u64.to_le_bytes()); // SEEK_SET
            if reply_val(&call_port(port, seek)) < 0 { return false; }
            let mut done = 0usize;
            while done < buf.len() {
                let mut rd = Message::empty();
                rd.tag = VFS_READ;
                rd.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
                rd.data[8..16]
                    .copy_from_slice(&(buf.as_mut_ptr() as u64 + done as u64).to_le_bytes());
                rd.data[16..24].copy_from_slice(&((buf.len() - done) as u64).to_le_bytes());
                let n = reply_val(&call_port(port, rd));
                if n < 0 { return false; }
                if n == 0 { break; } // hole / EOF: the zeros already in `buf`
                done += n as usize;
            }
            true
        }
        VnodeKind::TmpFile { idx, .. } => {
            let tmp = TMP_FILES.lock();
            if !tmp[idx].in_use { return false; }
            let len = tmp[idx].len as u64;
            if off < len {
                let n = ((len - off) as usize).min(buf.len());
                buf[..n].copy_from_slice(&tmp[idx].data[off as usize..off as usize + n]);
            }
            true
        }
        _ => false,
    }
}

fn backing_write(kind: VnodeKind, off: u64, buf: &[u8]) -> bool {
    match kind {
        VnodeKind::MountedFile { port, file_id } => {
            let mut seek = Message::empty();
            seek.tag = VFS_LSEEK;
            seek.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            seek.data[8..16].copy_from_slice(&off.to_le_bytes());
            seek.data[16..24].copy_from_slice(&0u64.to_le_bytes());
            if reply_val(&call_port(port, seek)) < 0 { return false; }
            let mut done = 0usize;
            while done < buf.len() {
                let mut wr = Message::empty();
                wr.tag = VFS_WRITE;
                wr.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
                wr.data[8..16]
                    .copy_from_slice(&(buf.as_ptr() as u64 + done as u64).to_le_bytes());
                wr.data[16..24].copy_from_slice(&((buf.len() - done) as u64).to_le_bytes());
                let n = reply_val(&call_port(port, wr));
                if n <= 0 { return false; }
                done += n as usize;
            }
            true
        }
        VnodeKind::TmpFile { idx, .. } => {
            let mut tmp = TMP_FILES.lock();
            if !tmp[idx].in_use { return false; }
            let end = off as usize + buf.len();
            if end > MAX_TMP_SIZE { return false; }
            tmp[idx].data[off as usize..end].copy_from_slice(buf);
            if end > tmp[idx].len { tmp[idx].len = end; }
            true
        }
        _ => false,
    }
}

/// The backing binding of loop `n`, copied out from under the lock.
fn loop_backing(n: u8) -> Option<VnodeKind> {
    let l = LOOPS.lock();
    let s = l.get(n as usize)?;
    if !s.bound { return None; }
    Some(s.backing)
}

// ── Public byte I/O ──────────────────────────────────────────────────────────

/// Largest single transfer we will bounce through the heap, matching the
/// virtio driver's own 256 KiB DMA window so one call never asks the buddy
/// allocator for a bigger contiguous run than the device path already uses.
/// A larger request is a short read/write, which `read_exact`/`write_all`
/// loop over — the same thing a real block device does.
const MAX_XFER: usize = 256 * 1024;

/// `read(2)` on a block node: copy `len` bytes at `off` into the caller's
/// buffer. `buf_ptr` is a user pointer and is touched with **no lock held**,
/// so a demand-paging fault there resolves normally.
pub fn read_at(dev: u16, off: u64, buf_ptr: usize, len: usize) -> isize {
    let (root, base, size) = match resolve(dev) { Some(r) => r, None => return -6 }; // ENXIO
    if off >= size { return 0; }
    let n = len.min((size - off) as usize).min(MAX_XFER);
    if n == 0 { return 0; }
    let mut v = alloc::vec![0u8; n];
    let ok = match root {
        Root::Disk(i) => disk_read(i as usize, base + off, &mut v),
        Root::Loop(l) => match loop_backing(l) {
            Some(k) => backing_read(k, base + off, &mut v),
            None => return -6, // ENXIO — unbound loop device
        },
    };
    if !ok { return -5; } // EIO
    unsafe { core::ptr::copy_nonoverlapping(v.as_ptr(), buf_ptr as *mut u8, n); }
    n as isize
}

/// `write(2)` on a block node. Writes past the end of the device are refused
/// with ENOSPC rather than silently truncated.
pub fn write_at(dev: u16, off: u64, buf_ptr: usize, len: usize) -> isize {
    let (root, base, size) = match resolve(dev) { Some(r) => r, None => return -6 };
    if off >= size { return -28; } // ENOSPC
    let n = len.min((size - off) as usize).min(MAX_XFER);
    if n == 0 { return 0; }
    let mut v = alloc::vec![0u8; n];
    unsafe { core::ptr::copy_nonoverlapping(buf_ptr as *const u8, v.as_mut_ptr(), n); }
    let ok = match root {
        Root::Disk(i) => disk_write(i as usize, base + off, &v),
        Root::Loop(l) => match loop_backing(l) {
            Some(k) => backing_write(k, base + off, &v),
            None => return -6,
        },
    };
    if !ok { return -5; }
    n as isize
}

/// `fsync(2)` on a block node.
pub fn flush_dev(dev: u16) -> isize {
    let (root, _, _) = match resolve(dev) { Some(r) => r, None => return -6 };
    match root {
        Root::Disk(i) => match ops() {
            Some(o) => if (o.flush)(i as usize) { 0 } else { -5 },
            None => -6,
        },
        // A loop device's durability is the backing file's; the mount server
        // owns that, and the writes above already went through it.
        Root::Loop(_) => 0,
    }
}

// ── /sys/class/block path parsing ────────────────────────────────────────────

const SYS_BLOCK: &[u8] = b"/sys/class/block";

/// Split `/sys/class/block/<a>[/<b>[/<c>]]` into at most three components.
/// Returns `None` for anything outside the tree or deeper than three.
fn sys_split(path: &[u8]) -> Option<(&[u8], Option<&[u8]>, Option<&[u8]>)> {
    let rest = path.strip_prefix(SYS_BLOCK)?;
    if rest.len() < 2 || rest[0] != b'/' { return None; }
    let mut parts: [&[u8]; 3] = [b"", b"", b""];
    let mut n = 0usize;
    for seg in rest[1..].split(|&c| c == b'/') {
        if seg.is_empty() || n == 3 { return None; }
        parts[n] = seg;
        n += 1;
    }
    match n {
        1 => Some((parts[0], None, None)),
        2 => Some((parts[0], Some(parts[1]), None)),
        3 => Some((parts[0], Some(parts[1]), Some(parts[2]))),
        _ => None,
    }
}

/// The subdirectory level named by `sub` under device `dev`, if any.
fn sub_level(dev: u16, e: &BDev, sub: &[u8]) -> Option<u8> {
    match sub {
        b"queue" => Some(LVL_QUEUE),
        // Partitions carry no `device` link on Linux either.
        b"device" => match e.backing {
            Backing::Part { .. } => None,
            _ => Some(LVL_DEVICE),
        },
        b"loop" => match e.backing {
            Backing::Loop(n) if LOOPS.lock()[n as usize].bound => Some(LVL_LOOP),
            _ => None,
        },
        _ => { let _ = dev; None }
    }
}

/// True when `path` is inside the synthesized sysfs tree at all.
pub fn is_sysfs_path(path: &[u8]) -> bool {
    path == b"/sys" || path.starts_with(b"/sys/")
}

/// Resolve `path` to a sysfs *directory*, as `(registry slot, level)`.
pub fn sysfs_dir(path: &[u8]) -> Option<(u16, u8)> {
    if path == b"/sys" { return Some((NO_DEV, LVL_SYS)); }
    if path == b"/sys/class" { return Some((NO_DEV, LVL_CLASS)); }
    if path == SYS_BLOCK { return Some((NO_DEV, LVL_BLOCK)); }
    let (a, b, c) = sys_split(path)?;
    if c.is_some() { return None; } // three components are always an attribute
    let devs = BDEVS.lock();
    let dev = find_name(&devs, a)?;
    let e = devs[dev as usize];
    drop(devs);
    match b {
        None => Some((dev, LVL_DEV)),
        Some(sub) => {
            if let Some(l) = sub_level(dev, &e, sub) { return Some((dev, l)); }
            // `/sys/class/block/<disk>/<part>` — a partition also appears as a
            // child of its disk, which is how disks-rs discovers it
            // (disks/src/disk.rs reads the disk's directory and re-resolves
            // every entry as a top-level partition node).
            let devs = BDEVS.lock();
            let p = find_name(&devs, sub)?;
            match devs[p as usize].backing {
                Backing::Part { parent, .. } if parent as u16 == dev => Some((p, LVL_DEV)),
                _ => None,
            }
        }
    }
}

// ── sysfs attribute generation ───────────────────────────────────────────────

fn put(out: &mut [u8], pos: usize, s: &[u8]) -> usize {
    let n = s.len().min(out.len().saturating_sub(pos));
    out[pos..pos + n].copy_from_slice(&s[..n]);
    pos + n
}

fn put_u64(out: &mut [u8], pos: usize, mut v: u64) -> usize {
    let mut d = [0u8; 20];
    let mut i = 0;
    if v == 0 { d[0] = b'0'; i = 1; }
    while v > 0 { d[i] = b'0' + (v % 10) as u8; i += 1; v /= 10; }
    let mut p = pos;
    while i > 0 { i -= 1; p = put(out, p, &d[i..i + 1]); }
    p
}

/// Generate the contents of a sysfs *attribute file*, or `None` when `path`
/// does not name one. The caller parks the bytes in an ephemeral pool slot.
pub fn sysfs_attr(path: &[u8], out: &mut [u8]) -> Option<usize> {
    let (a, b, c) = sys_split(path)?;
    let devs = BDEVS.lock();
    let dev = find_name(&devs, a)?;
    let e = devs[dev as usize];
    drop(devs);
    let sub = b?;
    match c {
        // `<name>/<key>` — an attribute of the device itself.
        None => attr_of(&e, sub, out),
        // `<name>/<subdir>/<leaf>` — queue/device/loop.
        Some(leaf) => {
            let level = sub_level(dev, &e, sub)?;
            sub_attr(&e, level, leaf, out)
        }
    }
}

/// `<name>/<sub>/<leaf>` attributes (queue/device/loop).
fn sub_attr(e: &BDev, level: u8, leaf: &[u8], out: &mut [u8]) -> Option<usize> {
    match level {
        LVL_QUEUE => match leaf {
            b"logical_block_size" | b"hw_sector_size" => {
                let p = put_u64(out, 0, e.lbs as u64);
                Some(put(out, p, b"\n"))
            }
            b"physical_block_size" => {
                let p = put_u64(out, 0, DISK_BLOCK as u64);
                Some(put(out, p, b"\n"))
            }
            b"minimum_io_size" => {
                let p = put_u64(out, 0, e.lbs as u64);
                Some(put(out, p, b"\n"))
            }
            b"rotational" => Some(put(out, 0, b"0\n")),
            _ => None,
        },
        LVL_DEVICE => match leaf {
            b"model" => Some(put(out, 0, match e.backing {
                Backing::Loop(_) => b"Loopback\n".as_slice(),
                _ => b"VirtIO Block Device\n".as_slice(),
            })),
            b"vendor" => Some(put(out, 0, b"LeandrOS\n")),
            _ => None,
        },
        LVL_LOOP => {
            let n = match e.backing { Backing::Loop(n) => n, _ => return None };
            match leaf {
                b"backing_file" => {
                    let l = LOOPS.lock();
                    let s = l[n as usize];
                    if !s.bound { return None; }
                    let plen = s.plen as usize;
                    let mut buf = [0u8; 128];
                    buf[..plen].copy_from_slice(&s.path[..plen]);
                    drop(l);
                    let p = put(out, 0, &buf[..plen]);
                    Some(put(out, p, b"\n"))
                }
                b"offset" | b"sizelimit" | b"autoclear" | b"partscan" => {
                    Some(put(out, 0, b"0\n"))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn attr_of(e: &BDev, key: &[u8], out: &mut [u8]) -> Option<usize> {
    match key {
        // ALWAYS 512-byte units, whatever the logical block size — this is
        // the one Linux quirk disks-rs hard-codes (`SECTOR_SIZE = 512`).
        b"size" => {
            let p = put_u64(out, 0, e.size / SECTOR);
            Some(put(out, p, b"\n"))
        }
        b"start" => match e.backing {
            Backing::Part { .. } => {
                let p = put_u64(out, 0, e.start / SECTOR);
                Some(put(out, p, b"\n"))
            }
            _ => None,
        },
        b"partition" => match e.backing {
            Backing::Part { pno, .. } => {
                let p = put_u64(out, 0, pno as u64);
                Some(put(out, p, b"\n"))
            }
            _ => None,
        },
        b"ro" | b"removable" | b"hidden" => Some(put(out, 0, b"0\n")),
        b"dev" => {
            let p = put_u64(out, 0, e.major as u64);
            let p = put(out, p, b":");
            let p = put_u64(out, p, e.minor as u64);
            Some(put(out, p, b"\n"))
        }
        _ => None,
    }
}

// ── sysfs directory enumeration ──────────────────────────────────────────────

/// The `idx`-th entry of sysfs directory `(dev, level)`. Returns the name
/// length written into `out` and whether the entry is itself a directory.
pub fn sysfs_dirent(dev: u16, level: u8, idx: usize, out: &mut [u8; NAME_MAX])
    -> Option<(usize, bool)>
{
    let emit = |out: &mut [u8; NAME_MAX], s: &[u8], is_dir: bool| {
        let n = s.len().min(NAME_MAX);
        out[..n].copy_from_slice(&s[..n]);
        Some((n, is_dir))
    };
    match level {
        LVL_SYS => if idx == 0 { emit(out, b"class", true) } else { None },
        LVL_CLASS => if idx == 0 { emit(out, b"block", true) } else { None },
        LVL_BLOCK => {
            let b = BDEVS.lock();
            let e = b.iter().filter(|e| e.in_use).nth(idx)?;
            let n = e.nlen as usize;
            out[..n].copy_from_slice(e.name());
            Some((n, true))
        }
        LVL_QUEUE => {
            const K: [&[u8]; 5] = [b"logical_block_size", b"physical_block_size",
                                   b"hw_sector_size", b"minimum_io_size", b"rotational"];
            emit(out, K.get(idx)?, false)
        }
        LVL_DEVICE => {
            const K: [&[u8]; 2] = [b"model", b"vendor"];
            emit(out, K.get(idx)?, false)
        }
        LVL_LOOP => {
            const K: [&[u8]; 4] = [b"backing_file", b"offset", b"sizelimit", b"autoclear"];
            emit(out, K.get(idx)?, false)
        }
        LVL_DEV => {
            let b = BDEVS.lock();
            let d = dev as usize;
            if d >= MAX_BDEV || !b[d].in_use { return None; }
            let backing = b[d].backing;
            drop(b);
            // Fixed attributes, then the per-kind ones, then child partitions.
            let mut i = idx;
            const BASE: [&[u8]; 4] = [b"size", b"ro", b"removable", b"dev"];
            if i < BASE.len() { return emit(out, BASE[i], false); }
            i -= BASE.len();
            if i == 0 { return emit(out, b"queue", true); }
            i -= 1;
            match backing {
                Backing::Part { .. } => {
                    const P: [&[u8]; 2] = [b"partition", b"start"];
                    if i < P.len() { return emit(out, P[i], false); }
                    i -= P.len();
                }
                _ => {
                    if i == 0 { return emit(out, b"device", true); }
                    i -= 1;
                    if let Backing::Loop(n) = backing {
                        if LOOPS.lock()[n as usize].bound {
                            if i == 0 { return emit(out, b"loop", true); }
                            i -= 1;
                        }
                    }
                }
            }
            // Child partitions — a partition must be visible BOTH here and at
            // the top level, because disks-rs reads this directory and then
            // re-resolves each entry name as `/sys/class/block/<entry>`.
            let b = BDEVS.lock();
            let e = b.iter().filter(|e| {
                e.in_use && matches!(e.backing, Backing::Part { parent, .. } if parent as u16 == dev)
            }).nth(i)?;
            let n = e.nlen as usize;
            out[..n].copy_from_slice(e.name());
            Some((n, true))
        }
        _ => None,
    }
}

// ── ioctls ───────────────────────────────────────────────────────────────────

// Block-layer ioctls (asm-generic, `_IO(0x12, n)`).
const BLKROSET: usize = 0x125D;
const BLKROGET: usize = 0x125E;
const BLKRRPART: usize = 0x125F;
const BLKGETSIZE: usize = 0x1260;
const BLKFLSBUF: usize = 0x1261;
const BLKSSZGET: usize = 0x1268;
const BLKPG: usize = 0x1269;
const BLKGETSIZE64: usize = 0x8008_1272;
const BLKPBSZGET: usize = 0x127B;
const BLKIOMIN: usize = 0x1278;
const BLKIOOPT: usize = 0x1279;
const BLKALIGNOFF: usize = 0x127A;
const BLKDISCARDZEROES: usize = 0x127C;

// Loop device ioctls.
const LOOP_SET_FD: usize = 0x4C00;
const LOOP_CLR_FD: usize = 0x4C01;
const LOOP_SET_STATUS64: usize = 0x4C04;
const LOOP_GET_STATUS64: usize = 0x4C05;
const LOOP_SET_CAPACITY: usize = 0x4C07;
const LOOP_CTL_ADD: usize = 0x4C80;
const LOOP_CTL_REMOVE: usize = 0x4C81;
const LOOP_CTL_GET_FREE: usize = 0x4C82;

const ENOTTY: isize = -25;

/// `struct blkpg_ioctl_arg` — `{ int op; int flags; int datalen; void *data; }`.
/// The pointer is 8-byte aligned on both our targets, so `data` sits at 16,
/// not 12, and the struct is 24 bytes.
const BLKPG_OP: usize = 0;
const BLKPG_DATA: usize = 16;
const BLKPG_ADD: i32 = 1;
const BLKPG_DEL: i32 = 2;

pub fn ioctl(pid: u32, dev: u16, cmd: usize, arg: usize) -> isize {
    if dev == LOOP_CONTROL { return loop_control_ioctl(cmd, arg); }
    let (backing, size, lbs) = {
        let b = BDEVS.lock();
        let d = dev as usize;
        if d >= MAX_BDEV || !b[d].in_use { return -6; }
        (b[d].backing, b[d].size, b[d].lbs)
    };

    match cmd {
        BLKGETSIZE64 => {
            if arg == 0 { return -14; }
            unsafe { (arg as *mut u64).write(size) };
            0
        }
        BLKGETSIZE => {
            if arg == 0 { return -14; }
            unsafe { (arg as *mut u64).write(size / SECTOR) };
            0
        }
        BLKSSZGET | BLKIOMIN => {
            if arg == 0 { return -14; }
            unsafe { (arg as *mut i32).write(lbs as i32) };
            0
        }
        BLKPBSZGET | BLKIOOPT => {
            if arg == 0 { return -14; }
            unsafe { (arg as *mut i32).write(DISK_BLOCK as i32) };
            0
        }
        BLKALIGNOFF | BLKDISCARDZEROES | BLKROGET => {
            if arg == 0 { return -14; }
            unsafe { (arg as *mut i32).write(0) };
            0
        }
        // We have no read-only devices, so the only legal request is "make it
        // writable"; anything else is EINVAL rather than a silent lie.
        BLKROSET => {
            if arg == 0 { return -14; }
            let v = unsafe { (arg as *const i32).read() };
            if v == 0 { 0 } else { -22 }
        }
        // The partition table is only ever changed through BLKPG here, so a
        // re-read has nothing to do — and succeeding is what Linux does for a
        // device with no partitions open.
        BLKRRPART => 0,
        BLKFLSBUF => flush_dev(dev),
        BLKPG => blkpg(dev, arg),
        LOOP_SET_FD | LOOP_CLR_FD | LOOP_SET_STATUS64 | LOOP_GET_STATUS64
        | LOOP_SET_CAPACITY => match backing {
            Backing::Loop(n) => loop_ioctl(pid, n, dev, cmd, arg),
            _ => ENOTTY,
        },
        _ => ENOTTY,
    }
}

fn loop_control_ioctl(cmd: usize, arg: usize) -> isize {
    match cmd {
        LOOP_CTL_GET_FREE => {
            let l = LOOPS.lock();
            match l.iter().position(|s| !s.bound) {
                Some(n) => n as isize,
                None => -24, // EMFILE — every loop device is in use
            }
        }
        // The node set is fixed at boot (see MAX_LOOPS), so "add" succeeds for
        // an index that already exists and "remove" for a free one; both are
        // what Linux reports when the state already matches the request.
        LOOP_CTL_ADD => {
            let n = arg as i64;
            if n < 0 || n as usize >= MAX_LOOPS { return -22; }
            if LOOPS.lock()[n as usize].bound { -16 } else { n as isize } // EBUSY
        }
        LOOP_CTL_REMOVE => {
            let n = arg as i64;
            if n < 0 || n as usize >= MAX_LOOPS { return -22; }
            if LOOPS.lock()[n as usize].bound { -16 } else { n as isize }
        }
        _ => ENOTTY,
    }
}

fn loop_ioctl(pid: u32, n: u8, dev: u16, cmd: usize, arg: usize) -> isize {
    match cmd {
        LOOP_SET_FD => {
            if LOOPS.lock()[n as usize].bound { return -16; } // EBUSY
            let backing_fd = arg;
            // Take our own reference on the file rather than borrowing the
            // caller's fd: the caller closes it the moment `attach` returns.
            let mut path = [0u8; 128];
            let plen = match fd_path_kernel(pid, backing_fd, &mut path) {
                Some(l) => l,
                None => return -9, // EBADF
            };
            let (kind, size) = match open_backing(&path[..plen]) {
                Some(v) => v,
                None => return -2, // ENOENT
            };
            {
                let mut l = LOOPS.lock();
                l[n as usize].bound = true;
                l[n as usize].backing = kind;
                l[n as usize].path[..plen].copy_from_slice(&path[..plen]);
                l[n as usize].plen = plen as u8;
            }
            set_size(dev, size);
            0
        }
        LOOP_CLR_FD => {
            let kind = {
                let mut l = LOOPS.lock();
                if !l[n as usize].bound { return -6; } // ENXIO
                let k = l[n as usize].backing;
                l[n as usize] = LoopState::empty();
                k
            };
            close_backing(kind);
            // The node stays; only the binding and the size go away.
            drop_partitions_of(dev);
            set_size(dev, 0);
            0
        }
        // `struct loop_info64` is 232 bytes. We keep no per-loop state beyond
        // the binding, so SET is accepted (and ignored — a zeroed info is
        // exactly what the caller sends to force a capacity refresh) and GET
        // reports the binding we do have.
        LOOP_SET_STATUS64 => {
            if arg == 0 { return -14; }
            if !LOOPS.lock()[n as usize].bound { return -6; }
            0
        }
        LOOP_GET_STATUS64 => {
            if arg == 0 { return -14; }
            let (bound, path, plen) = {
                let l = LOOPS.lock();
                (l[n as usize].bound, l[n as usize].path, l[n as usize].plen as usize)
            };
            if !bound { return -6; }
            let mut info = alloc::vec![0u8; 232];
            info[40..44].copy_from_slice(&(n as u32).to_le_bytes()); // lo_number
            let c = plen.min(63);
            info[56..56 + c].copy_from_slice(&path[..c]); // lo_file_name
            unsafe {
                core::ptr::copy_nonoverlapping(info.as_ptr(), arg as *mut u8, 232);
            }
            0
        }
        LOOP_SET_CAPACITY => {
            let kind = match loop_backing(n) { Some(k) => k, None => return -6 };
            set_size(dev, backing_size(kind));
            0
        }
        _ => ENOTTY,
    }
}

fn set_size(dev: u16, size: u64) {
    let mut b = BDEVS.lock();
    let d = dev as usize;
    if d < MAX_BDEV && b[d].in_use { b[d].size = size; }
}

/// Read `fd`'s path into a kernel buffer, reusing the VFS's own `fd_path`
/// machinery. Legal with a kernel destination: every server in this system
/// runs in the caller's address space (`ipc::port::send` dispatches
/// registered handlers synchronously on this thread), so a kernel pointer is
/// just another valid address.
fn fd_path_kernel(pid: u32, fd: usize, out: &mut [u8; 128]) -> Option<usize> {
    let ptr = out.as_mut_ptr() as usize;
    let len = out.len();
    let r = reply_val(&handle_fd_path(pid, fd, ptr, len));
    if r <= 0 { None } else { Some((r as usize).min(len)) }
}

/// Open `path` privately for a loop binding. Returns the vnode and its size.
fn open_backing(path: &[u8]) -> Option<(VnodeKind, u64)> {
    // tmpfs first, mirroring `tmpfs_path`'s precedence over the mount table
    // (after pivot_root every absolute path matches a mount prefix).
    if let Some(tp) = tmpfs_path(path) {
        let tmp = TMP_FILES.lock();
        let idx = tmp_find(&tmp[..], tp)?;
        let owner = tmp_owner(&tmp[..], idx);
        let len = tmp[owner].len as u64;
        return Some((VnodeKind::TmpFile { idx: owner, pos: 0, writable: true }, len));
    }
    let port = find_mount_port(path)?;
    // NUL-terminate for the mount server's own `read_cstr`.
    let mut cpath = alloc::vec![0u8; path.len() + 1];
    cpath[..path.len()].copy_from_slice(path);
    let mut open = Message::empty();
    open.tag = VFS_OPEN;
    open.data[0..8].copy_from_slice(&(cpath.as_ptr() as u64).to_le_bytes());
    open.data[8..16].copy_from_slice(&(O_RDWR as u64).to_le_bytes());
    open.data[16..24].copy_from_slice(&0u64.to_le_bytes());
    let fid = reply_val(&call_port(port, open));
    if fid < 0 { return None; }
    let kind = VnodeKind::MountedFile { port, file_id: fid as u32 };
    Some((kind, backing_size(kind)))
}

fn backing_size(kind: VnodeKind) -> u64 {
    match kind {
        VnodeKind::MountedFile { port, file_id } => {
            let mut m = Message::empty();
            m.tag = VFS_LSEEK;
            m.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            m.data[8..16].copy_from_slice(&0u64.to_le_bytes());
            m.data[16..24].copy_from_slice(&2u64.to_le_bytes()); // SEEK_END
            let r = reply_val(&call_port(port, m));
            if r < 0 { 0 } else { r as u64 }
        }
        VnodeKind::TmpFile { idx, .. } => TMP_FILES.lock()[idx].len as u64,
        _ => 0,
    }
}

fn close_backing(kind: VnodeKind) {
    if let VnodeKind::MountedFile { port, file_id } = kind {
        let mut m = Message::empty();
        m.tag = VFS_CLOSE;
        m.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
        let _ = call_port(port, m);
    }
}

// ── BLKPG ────────────────────────────────────────────────────────────────────

fn blkpg(dev: u16, arg: usize) -> isize {
    if arg == 0 { return -14; }
    let op = unsafe { ((arg + BLKPG_OP) as *const i32).read() };
    let data = unsafe { ((arg + BLKPG_DATA) as *const u64).read() } as usize;
    if data == 0 { return -14; }
    // struct blkpg_partition { long long start; long long length; int pno; ... }
    let start = unsafe { (data as *const i64).read() };
    let length = unsafe { (data as *const i64).add(1).read() };
    let pno = unsafe { ((data + 16) as *const i32).read() };
    if pno <= 0 { return -22; }

    match op {
        BLKPG_ADD => add_partition(dev, pno as u32, start, length),
        BLKPG_DEL => del_partition(dev, pno as u32),
        // BLKPG_RESIZE_PARTITION (3) and the newer ops are not implemented;
        // Linux answers EINVAL for an op it does not know.
        _ => -22,
    }
}

/// Partition node name: `<disk>N`, or `<disk>pN` when the disk name already
/// ends in a digit (`loop0` → `loop0p1`) — the udev rule every tool assumes.
fn part_name(disk: &[u8], pno: u32, out: &mut [u8; NAME_MAX]) -> Option<usize> {
    let mut n = disk.len();
    if n >= NAME_MAX { return None; }
    out[..n].copy_from_slice(disk);
    if disk[n - 1].is_ascii_digit() {
        if n + 1 >= NAME_MAX { return None; }
        out[n] = b'p';
        n += 1;
    }
    let mut d = [0u8; 10];
    let mut i = 0;
    let mut v = pno;
    if v == 0 { d[0] = b'0'; i = 1; }
    while v > 0 { d[i] = b'0' + (v % 10) as u8; i += 1; v /= 10; }
    if n + i > NAME_MAX { return None; }
    while i > 0 { i -= 1; out[n] = d[i]; n += 1; }
    Some(n)
}

fn add_partition(dev: u16, pno: u32, start: i64, length: i64) -> isize {
    if start < 0 || length <= 0 { return -22; }
    let (start, length) = (start as u64, length as u64);
    let mut b = BDEVS.lock();
    let d = dev as usize;
    if d >= MAX_BDEV || !b[d].in_use { return -6; }
    // Only a whole device can carry partitions.
    if let Backing::Part { .. } = b[d].backing { return -22; }
    if start + length > b[d].size { return -22; }
    // Existing partition with the same number, or an overlap: Linux answers
    // EBUSY for both, and `remove_kernel_partitions` runs first precisely so
    // that this cannot fire on the happy path.
    for e in b.iter() {
        if !e.in_use { continue; }
        if let Backing::Part { parent, pno: p } = e.backing {
            if parent as u16 != dev { continue; }
            if p == pno { return -16; }
            if start < e.start + e.size && e.start < start + length { return -16; }
        }
    }
    let mut name = [0u8; NAME_MAX];
    let nlen = match part_name(b[d].name(), pno, &mut name) { Some(n) => n, None => return -22 };
    let (major, base_minor, lbs) = (b[d].major, b[d].minor, b[d].lbs);
    let slot = match b.iter().position(|e| !e.in_use) { Some(s) => s, None => return -28 };
    b[slot] = BDev {
        in_use: true,
        name,
        nlen: nlen as u8,
        backing: Backing::Part { parent: dev as u8, pno },
        start,
        size: length,
        lbs,
        major,
        minor: base_minor + pno,
    };
    0
}

fn del_partition(dev: u16, pno: u32) -> isize {
    let mut b = BDEVS.lock();
    let found = b.iter().position(|e| {
        e.in_use && matches!(e.backing, Backing::Part { parent, pno: p }
                             if parent as u16 == dev && p == pno)
    });
    match found {
        Some(i) => { b[i] = BDev::empty(); 0 }
        None => -6, // ENXIO, as Linux reports for a partition that is not there
    }
}

/// Drop every partition node of `dev` (a loop detach invalidates them all).
fn drop_partitions_of(dev: u16) {
    let mut b = BDEVS.lock();
    for e in b.iter_mut() {
        if e.in_use && matches!(e.backing, Backing::Part { parent, .. } if parent as u16 == dev) {
            *e = BDev::empty();
        }
    }
}
