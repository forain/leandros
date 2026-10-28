//! ACPI laptop power: battery (`_BIF`/`_BIX`/`_BST`), AC adapter (`_PSR`),
//! lid (`_LID`), power/sleep buttons and ACPI video brightness (`_BCL`/`_BCM`),
//! evaluated by a small AML interpreter (`aml`) over the DSDT/SSDTs, with the
//! embedded controller (`ec`) behind the EmbeddedControl operation regions.
//!
//! Consumers (the `/sys/class/power_supply` and `/sys/class/backlight`
//! overlay in the VFS, the ACPI input device in evdev) only see the plain API
//! below. Everything here is inert when the firmware describes no battery,
//! adapter, lid or button: `battery_count() == 0`, `ac_present() == false`.
//!
//! Locking: the interpreter state lives behind one spin mutex; every public
//! call takes it for the length of one method evaluation (EC transactions
//! are microseconds each, a full `_BST` is well under a millisecond on
//! real hardware). Never call these from interrupt context.

extern crate alloc;
use alloc::string::String;

pub mod aml;
pub mod ec;
mod power;

/// `status` values (Linux POWER_SUPPLY_STATUS_*).
pub const ST_UNKNOWN: u8 = 0;
pub const ST_CHARGING: u8 = 1;
pub const ST_DISCHARGING: u8 = 2;
pub const ST_NOT_CHARGING: u8 = 3;
pub const ST_FULL: u8 = 4;

/// One battery as Linux's `drivers/acpi/battery.c` reports it.
#[derive(Clone, Default, Debug)]
pub struct BatteryInfo {
    /// `_STA` bit 4.
    pub present: bool,
    /// ST_* above.
    pub status: u8,
    /// 0..=100, from remaining / last-full.
    pub capacity: u32,
    /// `_BIF` power unit: true = energy (µWh / µW), false = charge (µAh / µA).
    pub energy_units: bool,
    /// Remaining capacity (µWh or µAh).
    pub now: u64,
    /// Last full charge capacity (µWh or µAh).
    pub full: u64,
    /// Design capacity (µWh or µAh).
    pub full_design: u64,
    /// Present rate (µW or µA), absolute value.
    pub rate: u64,
    /// Present voltage, µV.
    pub voltage_now: u64,
    /// Design voltage, µV.
    pub voltage_min_design: u64,
    /// `_BIX` cycle count (0 when only `_BIF` exists).
    pub cycle_count: u64,
    /// `_BIF` battery technology: 1 = rechargeable ("Li-ion" reported).
    pub rechargeable: bool,
    pub model: String,
    pub serial: String,
    pub kind: String,
    pub manufacturer: String,
}

/// Load the ACPI namespace and find the power devices. x86_64 only (the ARM
/// boards have no ACPI power devices); a no-op elsewhere. Call once, after the
/// FADT has been parsed and before anything reads the API below.
pub fn init(rsdp_phys: u64, hhdm_offset: u64) {
    power::init(rsdp_phys, hhdm_offset)
}

/// The `idx`-th raw ACPI table as (Linux file name, bytes).
pub fn raw_table(idx: usize) -> Option<(alloc::string::String, &'static [u8])> { power::raw_table(idx) }
/// Evaluate arbitrary AML under the interpreter lock (task context only).
pub fn with_ns<T>(f: impl FnOnce(&mut aml::Ns, &mut dyn aml::Host) -> T) -> Option<T> { power::with_ns(f) }

/// Number of batteries (`PNP0C0A` devices) the firmware describes.
pub fn battery_count() -> usize { power::battery_count() }

/// Evaluate battery `idx` live (`_STA`, `_BST`; `_BIX`/`_BIF` cached until
/// the battery is re-inserted or Notify(0x81) arrives).
pub fn battery(idx: usize) -> Option<BatteryInfo> { power::battery(idx) }

/// An AC adapter (`ACPI0003`) exists.
pub fn ac_present() -> bool { power::ac_present() }

/// `_PSR` of the AC adapter, live. None without an adapter or on error.
pub fn ac_online() -> Option<bool> { power::ac_online() }

/// Lid state from `_LID` (true = open). None without a lid device.
pub fn lid_open() -> Option<bool> { power::lid_open() }

/// ACPI video brightness: `(current level index, max index)` over the `_BCL`
/// list (levels after the two "AC/battery default" entries). None without `_BCM`.
pub fn acpi_backlight() -> Option<(u32, u32)> { power::acpi_backlight() }

/// Set the ACPI video brightness by index into the `_BCL` level list.
pub fn acpi_backlight_set(idx: u32) -> bool { power::acpi_backlight_set(idx) }

/// Where input events go: `(ev_type, code, value)` with Linux codes
/// (EV_KEY=1 KEY_POWER=116 KEY_SLEEP=142 KEY_BRIGHTNESSDOWN=224
/// KEY_BRIGHTNESSUP=225; EV_SW=5 SW_LID=0). The callee must not block and
/// must not call back into this module. A key press arrives as value 1 then 0;
/// a SYN_REPORT is the callee's job.
pub fn set_input_sink(f: fn(u16, u16, i32)) { power::set_input_sink(f) }

/// Service ACPI events: fixed power/sleep button status, GPEs, EC queries
/// (`_Qxx`), and the Notify()s they raise (lid, buttons, battery/AC changes,
/// video brightness keys). Call every ~100–250 ms from a kernel thread.
/// Cheap (a few port reads) when nothing is pending.
pub fn poll() { power::poll() }

/// Monotonic counter bumped on every battery/AC Notify, so pollers (upower)
/// can tell a change happened without re-reading everything.
pub fn change_seq() -> u64 { power::change_seq() }

pub use power::SleepAml;
/// `_TTS(state)`, `_PTS(state)`, `\_SI._SST(3)`; logs every method result and OpRegion access summary.
pub fn sleep_prepare(state: u8) -> SleepAml { power::sleep_prepare(state) }
/// `\_SI._SST(2)`, `_WAK(state)`, `\_SI._SST(1)`.
pub fn sleep_wake(state: u8) -> SleepAml { power::sleep_wake(state) }
/// `_TTS(state)` alone.
pub fn tts(state: u8) { power::tts(state) }
/// `\_S0_`/`\_S3_`/`\_S5_` package -> (SLP_TYPa, SLP_TYPb).
pub fn sx_typ(name: &str) -> Option<(u8, u8)> { power::sx_typ(name) }
/// Stop/resume kacpid's EC-query and GPE evaluation.
pub fn pause_poll(on: bool) { power::pause_poll(on) }
/// Diagnostic: evaluate `_PTS(3)` (and `_WAK(3)` when `with_wak`) without ever entering a sleep state.
pub fn s3_dryrun(with_wak: bool) { power::s3_dryrun(with_wak) }
