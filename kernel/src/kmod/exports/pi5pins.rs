//! kapi export: the Pi 5 Wi-Fi pins (SDIO2 mux, WL_REG_ON), for the module named `brcmf` only.
use super::*;

/// See `kmod_abi::plat::sys_plat::kapi_pi5_pins`. -1 for any other module, -22 bad op or missing `out`.
#[no_mangle]
pub extern "C" fn kapi_pi5_pins(owner: KmodId, op: u32, arg: u32, out: *mut u64) -> i32 {
    use kmod_abi::plat::pi5_pins as P;
    if crate::kmod::name_str(&crate::kmod::registry::name_of(owner)) != "brcmf" { return -1; }
    if op > P::WL_ON_READY || (op == P::MUX_SDIO2 && out.is_null()) { return -22; }
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    {
        use drivers::bcm2712_gpio::{self as gpio, Block, Chip, GROUP_SDIO2_WL_ON, WL_ON};
        use drivers::bcm2712_id::{self, Stepping};
        match op {
            P::WL_ON_SET => { let _ = gpio::gpio_set_output(Chip::Gio, WL_ON, arg != 0); return 0; }
            P::WL_ON_READY => return if gpio::gpio_is_output(Chip::Gio, WL_ON).is_ok() { 0 } else { -19 },
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
