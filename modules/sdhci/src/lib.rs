//! SDHCI SD card (Raspberry Pi 5 / BCM2712, QEMU raspi4b) as a loadable module: the SAME driver source as
//! the built-in (`drivers/src/sdhci.rs`) with `kenv` pointing at the kmod-kapi shim. The card goes through
//! `kapi_blk_register` (class "vd", so root=/dev/vdX and root=LABEL= work through the runtime registry).
//! The board is a build feature (`rpi5` / `raspi4b`, passed by kmod-build.sh). Autoload by DT compatible
//! (the Pi 5 SD controller / the BCM2711 EMMC2) and listed in modules/BOOT for root-on-SD.
#![no_std]

extern crate alloc;
use core::sync::atomic::{AtomicBool, Ordering};

mod kenv {
    pub use kmod_kapi::blk::env::*;
}

// The driver source names the kernel's `crate::kdiag`.
mod kdiag {
    pub use kmod_kapi::kdiag::*;
}

// Pi 5 only: the UHS-I voltage switch drives two gio_aon lines (`kapi_pi5_pins`) and the card-MBR parser is pure logic.
#[cfg(feature = "rpi5")]
mod bcm2712_gpio {
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Chip { Gio, GioAon }
    pub fn gpio_get(chip: Chip, line: u32) -> Result<bool, i32> {
        if chip != Chip::GioAon { return Err(-19); }
        kmod_kapi::plat::pi5_aon_get(line)
    }
    pub fn gpio_set_output(chip: Chip, line: u32, on: bool) -> Result<(), i32> {
        if chip != Chip::GioAon { return Err(-19); }
        kmod_kapi::plat::pi5_aon_set(line, on)
    }
}
#[cfg(feature = "rpi5")]
#[path = "../../../drivers/src/rpi_misc_logic.rs"]
#[allow(dead_code)]
mod rpi_misc_logic;

#[path = "../../../drivers/src/blockdev.rs"]
mod blockdev;
#[path = "../../../drivers/src/partscan.rs"]
mod partscan;
#[path = "../../../drivers/src/blkmod.rs"]
mod blkdev;
#[path = "../../../drivers/src/sdhci.rs"]
mod sdhci;

/// What `sdhci::info` returns on the kernel side (`drivers::BlkDevInfo`).
pub struct BlkDevInfo {
    pub total_blocks: u64,
    pub block_size: u32,
    pub fstype: Option<&'static str>,
}

kmod_rt::module! {
    name: "sdhci",
    init: init,
    of: ["brcm,bcm2712-sdhci", "brcm,bcm2711-emmc2"]
}

/// The SD card as a `BlockDevice`: 4096-byte logical blocks, the way the driver's own block API is shaped.
struct Card { idx: usize, blocks: u64 }

impl blockdev::BlockDevice for Card {
    fn sector_size(&self) -> u32 { 4096 }
    fn capacity_sectors(&self) -> u64 { self.blocks }
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), blockdev::BlkErr> {
        if buf.len() % 4096 != 0 { return Err(blockdev::BlkErr::Io); }
        if sdhci::read_blocks(self.idx, lba, buf) { Ok(()) } else { Err(blockdev::BlkErr::Io) }
    }
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), blockdev::BlkErr> {
        if buf.len() % 4096 != 0 { return Err(blockdev::BlkErr::Io); }
        if sdhci::write_blocks(self.idx, lba, buf) { Ok(()) } else { Err(blockdev::BlkErr::Io) }
    }
    fn flush(&self) -> Result<(), blockdev::BlkErr> { if sdhci::flush(self.idx) { Ok(()) } else { Err(blockdev::BlkErr::Io) } }
    fn removable(&self) -> bool { false }
    fn name_class(&self) -> &'static str { "vd" }
}

/// The driver does not read C_SIZE yet (`sdhci::info` reports 0). Claim 256 GiB of 4 KiB blocks so the registry's
/// bounds checks pass; reads past the real end fail in the driver.
const UNKNOWN_BLOCKS: u64 = 1 << 26;

static DONE: AtomicBool = AtomicBool::new(false);

fn init() -> i32 {
    if DONE.swap(true, Ordering::SeqCst) { return -16; }
    sdhci::configure(kmod_kapi::cmdline());
    sdhci::init();
    let mut found = 0;
    for idx in 0..sdhci::device_count() {
        let Some(info) = sdhci::info(idx) else { continue };
        let card = alloc::boxed::Box::leak(alloc::boxed::Box::new(Card { idx, blocks: if info.total_blocks != 0 { info.total_blocks } else { UNKNOWN_BLOCKS } }));
        if blkdev::register(card).is_ok() { found += 1; }
    }
    if found == 0 { -19 } else { 0 }
}
