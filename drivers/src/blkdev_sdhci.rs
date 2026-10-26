//! `drivers::blkdev` on rpi5/raspi4b: the SD-card driver, selected at compile
//! time exactly as before (no PCI, no USB, no runtime registry on these
//! boards). The block façade is `sdhci`'s own functions, re-exported
//! unchanged; this file only adds the name/root helpers the kernel's
//! `sys_mount` and the VFS `DiskOps` call on every build, answering them with
//! the fixed virtio-style naming the SD driver already mirrors (the card is
//! index 1, i.e. `vdb` — see `sdhci::init`).

pub use crate::sdhci::{device_count, flush, has_f2fs, info, init, read_block, read_blocks, write_block};
pub use crate::blockdev::{
    is_f2fs_block0, parse_root_arg, BlkErr, BlockDevice, BootDisk, DiskDesc, RootErr, NAME_MAX,
};

const PATHS: [&str; 8] = [
    "/dev/vda", "/dev/vdb", "/dev/vdc", "/dev/vdd",
    "/dev/vde", "/dev/vdf", "/dev/vdg", "/dev/vdh",
];

/// Index of `vd<letter>`, if that index exists.
pub fn lookup_name(name: &str) -> Option<usize> {
    let b = name.as_bytes();
    if b.len() == 3 && &b[..2] == b"vd" && b[2].is_ascii_lowercase() {
        let i = (b[2] - b'a') as usize;
        if i < device_count() { return Some(i); }
    }
    None
}

pub fn dev_path(idx: usize) -> &'static str {
    PATHS.get(idx).copied().unwrap_or("/dev/?")
}

pub fn describe(idx: usize) -> Option<DiskDesc> {
    if idx >= device_count() || idx >= PATHS.len() { return None; }
    let mut name = [0u8; NAME_MAX];
    name[..3].copy_from_slice(&PATHS[idx].as_bytes()[5..8]);
    Some(DiskDesc {
        name,
        nlen: 3,
        parent: None,
        pno: 0,
        start_bytes: 0,
        bytes: info(idx).map_or(0, |i| i.total_blocks * 4096),
        sector_size: 512,
        removable: false,
        read_only: false,
        gone: false,
        class: "vd",
        ordinal: idx as u16,
    })
}

static HINT_WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// True if the card at `idx` is f2fs with volume label `label` (UTF-16LE
/// `volume_name`, byte 124 of the superblock; ASCII labels only).
fn label_is(idx: usize, label: &str) -> bool {
    const NAME: usize = 1024 + 124;
    let l = label.as_bytes();
    if l.is_empty() || l.len() >= 512 || idx >= device_count() { return false; }
    let mut buf = alloc::vec![0u8; 4096];
    if !read_blocks(idx, 0, &mut buf) || !is_f2fs_block0(&buf) { return false; }
    let u = |i: usize| u16::from_le_bytes([buf[NAME + 2 * i], buf[NAME + 2 * i + 1]]);
    l.iter().enumerate().all(|(i, &c)| u(i) == c as u16) && u(l.len()) == 0
}

/// `/dev/root` on this board: `root=/dev/vdX` or `root=LABEL=<f2fs label>`
/// when it names the card, else the card at index 1 if it holds f2fs.
///
/// The card is probed synchronously in `sdhci::init`, before userspace runs,
/// and nothing hotplugs here, so a `root=` that names nothing now never will.
/// Answering `NotFound` for it made init wait its full 30 s for a root that
/// was already present: with no cmdline.txt the Pi firmware appends its own
/// default `root=/dev/mmcblk0p2 rootfstype=ext4 rootwait`. Such a hint is
/// logged once and the card is used.
pub fn resolve_root(cmdline: Option<&str>, _boot: Option<BootDisk>) -> Result<usize, RootErr> {
    if let Some(spec) = cmdline.and_then(parse_root_arg) {
        if let Some(i) = spec.strip_prefix("/dev/").and_then(lookup_name) { return Ok(i); }
        if let Some(label) = spec.strip_prefix("LABEL=") {
            if label_is(1, label) { return Ok(1); }
        }
        if !HINT_WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            crate::pci::serial_debug("[BLK] root=");
            crate::pci::serial_debug(spec);
            crate::pci::serial_debug(" names no device on this board; using the SD card\n");
        }
    }
    if 1 < device_count() && has_f2fs(1) { Ok(1) } else { Err(RootErr::NotFound) }
}

/// The SD path has no runtime registration; always `Unsupported`.
pub fn register(_dev: &'static dyn BlockDevice) -> Result<usize, BlkErr> {
    Err(BlkErr::Unsupported)
}

/// No log partition support on the SD-card boards.
pub fn find_log_partition(_boot: Option<BootDisk>) -> Option<usize> { None }
pub fn raw_geometry(_idx: usize) -> Option<(u32, u64)> { None }
pub fn raw_read(_idx: usize, _byte_off: u64, _buf: &mut [u8]) -> bool { false }
pub fn raw_write(_idx: usize, _byte_off: u64, _buf: &[u8], _panic_path: bool) -> bool { false }
pub fn raw_flush(_idx: usize) -> bool { false }
/// The SD card is never read-only here.
pub fn read_only(_idx: usize) -> bool { false }
/// The SD card is fixed and USB disks never register on this build, so
/// there is nothing to unplug.
pub fn remove_disk(_idx: usize) -> usize { 0 }

/// Multi-block write: the controller is driven one block at a time (PIO),
/// so this is a loop over `write_block`. `false` on a short buffer or the
/// first failing block.
pub fn write_blocks(dev_idx: usize, blk: u64, buf: &[u8]) -> bool {
    const BS: usize = 4096;
    if buf.is_empty() || buf.len() % BS != 0 { return false; }
    buf.chunks_exact(BS).enumerate().all(|(i, b)| {
        write_block(dev_idx, blk + i as u64, b.try_into().unwrap())
    })
}

/// Nothing is ever unplugged on this build (see `remove_disk`).
pub fn removed_generation() -> usize { 0 }

/// Flush every SD device before reboot/poweroff: `(ok, failed)`.
pub fn flush_all_disks() -> (usize, usize) {
    let (mut ok, mut bad) = (0, 0);
    for i in 0..device_count() {
        if flush(i) { ok += 1; } else { bad += 1; }
    }
    (ok, bad)
}

/// `LABEL=` / `PARTUUID=` mount sources are not resolved on the SD-card
/// build (no partition registry): `None` lets the caller fall back to a
/// plain device path.
pub fn resolve_source(_spec: &str, _cmdline: Option<&str>, _boot: Option<BootDisk>)
    -> Option<Result<usize, RootErr>>
{
    None
}
