//! kapi export: the Pi 5 Wi-Fi pins (SDIO2 mux, WL_REG_ON), for the module named `brcmf` only, and the SD host's
//! two gio_aon lines (vqmmc 1.8 V select, card power) as high-level ops, for the module named `sdhci` only.
//!
//! The SD sets follow lane sdlock's rule through `sdgrant::pin_set_allowed`: the module may do whatever the built-in
//! driver does on a still-locked card (power cycle, 1.8 V after ACMD41) and nothing more. The card-write gate itself
//! is the shared `drivers/src/sdhci.rs` source (WRITE_GRANT / `sdgrant::issue`), compiled into the module.
use super::*;

/// See `kmod_abi::plat::sys_plat::kapi_pi5_pins`. -1 for any other module, -22 bad op or missing `out`.
#[no_mangle]
pub extern "C" fn kapi_pi5_pins(owner: KmodId, op: u32, arg: u32, out: *mut u64) -> i32 {
    use kmod_abi::plat::pi5_pins as P;
    // Normalise through the running generation, never trust the module-claimed id.
    let name = crate::kmod::registry::name_of(crate::kmod::gate::owner(owner));
    let who = crate::kmod::name_str(&name);
    if op > P::MAX_OP { return -22; }
    if who != P::module_for(op) { return -1; }
    if op == P::MUX_SDIO2 && out.is_null() { return -22; }
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    {
        use drivers::bcm2712_gpio::{self as gpio, Block, Chip, GROUP_SDIO2_WL_ON, WL_ON};
        use drivers::bcm2712_id::{self, Stepping};
        match op {
            P::WL_ON_SET => { let _ = gpio::gpio_set_output(Chip::Gio, WL_ON, arg != 0); return 0; }
            P::WL_ON_READY => return if gpio::gpio_is_output(Chip::Gio, WL_ON).is_ok() { 0 } else { -19 },
            P::SD_IO_1V8_GET | P::SD_IO_1V8_SET | P::SD_POWER_GET | P::SD_POWER_SET => {
                let Some((line, set)) = P::sd_line(op) else { return -22 };
                if set {
                    let io = line == P::GIO_AON_SD_IOVDD_SEL;
                    let pwr = gpio::gpio_get(Chip::GioAon, P::GIO_AON_SD_PWR_ON).unwrap_or(false);
                    if !drivers::rpi_misc_logic::sdgrant::pin_set_allowed(io, arg != 0, pwr) {
                        static LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
                        if !LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                            drivers::pci::serial_debug("[kmod] sdhci pins: refused 1.8 V I/O on an unpowered card\n");
                        }
                        return -1;
                    }
                }
                return if set { if gpio::gpio_set_output(Chip::GioAon, line, arg != 0).is_ok() { 0 } else { -19 } }
                else { match gpio::gpio_get(Chip::GioAon, line) { Ok(v) => v as i32, Err(_) => -19 } };
            }
            _ => {}
        }
        let st = bcm2712_id::stepping().stepping;
        if bcm2712_id::pin_rows(st).is_none() { return -19; }
        let mut before = 0u32;
        for (i, p) in (30..=35).enumerate() {
            before |= (gpio::pin_get_fsel(Block::Main, p).unwrap_or(0) & 0xF) << (i as u32 * 4);
        }
        return match gpio::pin_apply(Block::Main, &GROUP_SDIO2_WL_ON) {
            Ok(w) => {
                unsafe { out.write(if matches!(st, Stepping::D0) { 1 } else { 0 }); out.add(1).write(before as u64); out.add(2).write(w as u64); }
                0
            }
            Err(_) => -5,
        };
    }
    #[allow(unreachable_code)]
    { let _ = arg; -19 }
}
