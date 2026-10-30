//! kapi exports: Wi-Fi radio class
use super::*;

/// Register a Wi-Fi radio. The kernel owns the connection manager and runs it on its own kthread;
/// the table (`drivers::wifi_radio::KWifiRadioOps`) names the radio and carries its callbacks.
/// `legacy` may be null. Returns the registry index, or -errno (-22 bad table, -17 name taken,
/// -12 no thread, -19 no Wi-Fi stack in this kernel).
#[cfg(feature = "drv-wifi-stack")]
#[no_mangle]
pub extern "C" fn kapi_wifi_register(_owner: KmodId, ops: *const drivers::wifi_radio::KWifiRadioOps, ctx: usize,
                                     legacy: *const drivers::wifi_radio::KLegacy) -> i32 {
    // The module cannot reach the manager's `wifi.*` knobs; the built-in driver applies them in its init.
    drivers::wifi_pref::set_knobs(crate::boot_cmdline().unwrap_or(""));
    drivers::wifi_class::register_raw(ops, ctx, legacy)
}

#[cfg(not(feature = "drv-wifi-stack"))]
#[no_mangle]
pub extern "C" fn kapi_wifi_register(_owner: KmodId, _ops: *const u8, _ctx: usize, _legacy: *const u8) -> i32 { -19 }
