//! Power device discovery and evaluation (stub, filled in by lane lappower).
use super::BatteryInfo;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
static SINK: AtomicUsize = AtomicUsize::new(0);
static SEQ: AtomicU64 = AtomicU64::new(0);
pub fn init(_rsdp: u64, _hhdm: u64) {}
pub fn battery_count() -> usize { 0 }
pub fn battery(_i: usize) -> Option<BatteryInfo> { None }
pub fn ac_present() -> bool { false }
pub fn ac_online() -> Option<bool> { None }
pub fn lid_open() -> Option<bool> { None }
pub fn acpi_backlight() -> Option<(u32, u32)> { None }
pub fn acpi_backlight_set(_i: u32) -> bool { false }
pub fn set_input_sink(f: fn(u16, u16, i32)) { SINK.store(f as usize, Ordering::Release) }
pub fn poll() {}
pub fn change_seq() -> u64 { SEQ.load(Ordering::Relaxed) }
