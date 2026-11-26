//! Platform devices and board services (kmodpi P0): probe registration by `compatible`, the board
//! MAC / stepping / DTB queries, SError checkpoints, contiguous page aliases and kexec DMA quiesce hooks.

use kmod_abi::plat::{sys_plat, KDmaQuiesceFn, KPlatProbeFn};

pub use kmod_abi::plat::{pi5_pins, board_mac, dma_dir, quiesce_stage};
pub use kmod_abi::KPlatDev;

/// Register a driver for the NUL-free `compats`; every already-announced unclaimed device is offered to `probe` now.
pub fn register(compats: &[&str], probe: KPlatProbeFn, ctx: usize) -> Result<(), i32> {
    let mut buf = alloc::vec::Vec::new();
    for c in compats { buf.extend_from_slice(c.as_bytes()); buf.push(0); }
    match unsafe { sys_plat::kapi_plat_register(crate::owner(), buf.as_ptr(), buf.len(), probe, ctx) } { 0 => Ok(()), e => Err(e) }
}
pub fn unregister() { unsafe { sys_plat::kapi_plat_unregister(crate::owner()); } }
/// The board's device tree blob, if it has one.
pub fn dtb() -> Option<&'static [u8]> {
    let mut n = 0usize;
    let p = unsafe { sys_plat::kapi_dtb_blob(&mut n) };
    if p.is_null() || n == 0 { None } else { Some(unsafe { core::slice::from_raw_parts(p, n) }) }
}
/// Factory MAC for `board_mac::*`, None on boards without one.
pub fn mac(which: u32) -> Option<[u8; 6]> {
    let mut m = [0u8; 6];
    if unsafe { sys_plat::kapi_board_mac(which, m.as_mut_ptr()) } == 0 { Some(m) } else { None }
}
/// BCM2712 stepping: 0 C0, 1 D0, 2 unknown. None on other boards.
pub fn soc_stepping() -> Option<u32> { match unsafe { sys_plat::kapi_soc_stepping() } { v if v >= 0 => Some(v as u32), _ => None } }
/// Pending SError syndrome since the last checkpoint.
pub fn serror_checkpoint() -> Option<u64> { match unsafe { sys_plat::kapi_serror_checkpoint() } { 0 => None, s => Some(s) } }
/// Contiguous alias of `pages` (page addresses). cache: 0 UC, 1 WC, 2 WB.
pub fn vmap(pages: &[u64], cache: u32) -> Option<usize> {
    match unsafe { sys_plat::kapi_vmap(pages.as_ptr(), pages.len(), cache) } { 0 => None, v => Some(v) }
}
pub fn vunmap(va: usize) { unsafe { sys_plat::kapi_vunmap(va) } }
/// Register a kexec bus-master stop hook (`quiesce_stage::*`). Removed again if init fails.
pub fn dma_quiesce_register(name: &'static str, stage: u32, f: KDmaQuiesceFn, ctx: usize) -> Result<(), i32> {
    match unsafe { sys_plat::kapi_dma_quiesce_register(crate::owner(), name.as_ptr(), name.len(), stage, f, ctx) } { 0 => Ok(()), e => Err(e) }
}
/// Attach `h(ctx)` to RP1 interrupt source `n` (`level`: re-armed after each run). Err = -errno.
pub fn rp1_irq_request(n: u32, level: bool, h: kmod_abi::KIrqHandler, ctx: usize) -> Result<(), i32> {
    match unsafe { sys_plat::kapi_rp1_irq_request(crate::owner(), n, level, h, ctx) } { 0 => Ok(()), e => Err(e) }
}
pub fn rp1_irq_free(n: u32) { unsafe { sys_plat::kapi_rp1_irq_free(crate::owner(), n); } }

/// Result of the SDIO2 pin mux: stepping (0 C0, 1 D0), packed fsel nibbles of gpio30..35 before, register writes.
pub struct Sdio2Mux { pub stepping: u32, pub before: u32, pub writes: u32 }
/// Mux gpio28 and gpio30..35 for the Wi-Fi SDIO host. Err(-19) unknown stepping / not a Pi 5, Err(-1) not the brcmf module.
pub fn pi5_sdio2_mux() -> Result<Sdio2Mux, i32> {
    let mut o = [0u64; 3];
    match unsafe { sys_plat::kapi_pi5_pins(crate::owner(), pi5_pins::MUX_SDIO2, 0, o.as_mut_ptr()) } {
        0 => Ok(Sdio2Mux { stepping: o[0] as u32, before: o[1] as u32, writes: o[2] as u32 }),
        e => Err(e),
    }
}
/// Drive WL_REG_ON.
pub fn pi5_wl_on(on: bool) { unsafe { sys_plat::kapi_pi5_pins(crate::owner(), pi5_pins::WL_ON_SET, on as u32, core::ptr::null_mut()); } }
/// A gio_aon line (the SD host's vqmmc / card power; module `sdhci` only). Err(-19) unavailable, Err(-1) not that module.
pub fn pi5_aon_get(line: u32) -> Result<bool, i32> {
    match unsafe { sys_plat::kapi_pi5_pins(crate::owner(), pi5_pins::AON_GET, line, core::ptr::null_mut()) } { v if v >= 0 => Ok(v != 0), e => Err(e) }
}
pub fn pi5_aon_set(line: u32, on: bool) -> Result<(), i32> {
    match unsafe { sys_plat::kapi_pi5_pins(crate::owner(), pi5_pins::AON_SET, (line & 0xFF) | (on as u32) << 8, core::ptr::null_mut()) } { 0 => Ok(()), e => Err(e) }
}
/// The WL_REG_ON line is usable.
pub fn pi5_wl_on_ready() -> bool { unsafe { sys_plat::kapi_pi5_pins(crate::owner(), pi5_pins::WL_ON_READY, 0, core::ptr::null_mut()) == 0 } }
