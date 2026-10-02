//! org.freedesktop.UPower for a machine with no battery.
//!
//! LeandrOS has no power-supply class (no ACPI battery/AC driver, no
//! /sys/class/power_supply), which is exactly what a desktop PC looks like to
//! upower. This reports what upower itself reports there:
//!
//!   * no devices (EnumerateDevices is empty), OnBattery = false, no lid;
//!   * a DisplayDevice of Type Unknown, IsPresent = false, PowerSupply = false,
//!     icon "battery-missing-symbolic" (the composite device upower always
//!     exports, which battery widgets read to decide whether to show at all);
//!   * a KbdBacklight object with maximum brightness 0 (no keyboard light).
//!
//! Consumers in the session: cosmic-idle (OnBattery picks AC vs battery idle
//! timeouts), cosmic-osd and cosmic-settings-daemon (KbdBacklight, battery
//! notifications), cosmic-settings' Power page (battery section).

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

const NAME: &str = "org.freedesktop.UPower";
const PATH: &str = "/org/freedesktop/UPower";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const KBD_BACKLIGHT: &str = "/org/freedesktop/UPower/KbdBacklight";

struct UPower;

#[zbus::interface(name = "org.freedesktop.UPower")]
impl UPower {
    fn enumerate_devices(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    fn get_display_device(&self) -> OwnedObjectPath {
        ObjectPath::from_static_str_unchecked(DISPLAY_DEVICE).into()
    }

    /// What upower would do at critical battery. Never reached without one.
    fn get_critical_action(&self) -> &str {
        "PowerOff"
    }

    #[zbus(signal)]
    async fn device_added(emitter: &SignalEmitter<'_>, device: ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_removed(emitter: &SignalEmitter<'_>, device: ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn daemon_version(&self) -> &str {
        "1.90.6"
    }

    #[zbus(property)]
    fn on_battery(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn lid_is_closed(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn lid_is_present(&self) -> bool {
        false
    }
}

/// The composite "display device", as upower exports it when nothing in the
/// machine is a battery.
struct DisplayDevice {
    update_time: u64,
}

#[zbus::interface(name = "org.freedesktop.UPower.Device")]
impl DisplayDevice {
    fn refresh(&self) {}

    fn get_history(&self, _type: &str, _timespan: u32, _resolution: u32) -> Vec<(u32, f64, u32)> {
        Vec::new()
    }

    fn get_statistics(&self, _type: &str) -> Vec<(f64, f64)> {
        Vec::new()
    }

    #[zbus(property)]
    fn native_path(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn vendor(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn model(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn serial(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn update_time(&self) -> u64 {
        self.update_time
    }
    #[zbus(property, name = "Type")]
    fn type_(&self) -> u32 {
        0 // Unknown
    }
    #[zbus(property)]
    fn power_supply(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn has_history(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn has_statistics(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn online(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn energy(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn energy_empty(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn energy_full(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn energy_full_design(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn energy_rate(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn voltage(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn voltage_min_design(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn voltage_max_design(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn charge_cycles(&self) -> i32 {
        -1
    }
    #[zbus(property)]
    fn luminosity(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn time_to_empty(&self) -> i64 {
        0
    }
    #[zbus(property)]
    fn time_to_full(&self) -> i64 {
        0
    }
    #[zbus(property)]
    fn percentage(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn temperature(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn is_present(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn state(&self) -> u32 {
        0 // Unknown
    }
    #[zbus(property)]
    fn is_rechargeable(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn capacity(&self) -> f64 {
        0.0
    }
    #[zbus(property)]
    fn technology(&self) -> u32 {
        0 // Unknown
    }
    #[zbus(property)]
    fn warning_level(&self) -> u32 {
        1 // None
    }
    #[zbus(property)]
    fn battery_level(&self) -> u32 {
        0 // Unknown
    }
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "battery-missing-symbolic"
    }
    #[zbus(property)]
    fn charge_start_threshold(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn charge_end_threshold(&self) -> u32 {
        100
    }
    #[zbus(property)]
    fn charge_threshold_enabled(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn charge_threshold_supported(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn charge_threshold_settings_supported(&self) -> u32 {
        0
    }
}

/// No keyboard backlight: maximum 0, and the brightness never changes, so no
/// BrightnessChanged is ever emitted (that signal is what pops the OSD).
struct KbdBacklight;

#[zbus::interface(name = "org.freedesktop.UPower.KbdBacklight")]
impl KbdBacklight {
    fn get_brightness(&self) -> i32 {
        0
    }

    fn get_max_brightness(&self) -> i32 {
        0
    }

    fn set_brightness(&self, value: i32) -> zbus::fdo::Result<()> {
        if value == 0 {
            Ok(())
        } else {
            Err(zbus::fdo::Error::NotSupported("no keyboard backlight".into()))
        }
    }

    #[zbus(signal)]
    async fn brightness_changed(emitter: &SignalEmitter<'_>, value: i32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn brightness_changed_with_source(
        emitter: &SignalEmitter<'_>,
        value: i32,
        source: &str,
    ) -> zbus::Result<()>;
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    let (update_time, _) = crate::now_usec();
    let conn = builder
        .serve_at(PATH, UPower)?
        .serve_at(DISPLAY_DEVICE, DisplayDevice { update_time: update_time / 1_000_000 })?
        .serve_at(KBD_BACKLIGHT, KbdBacklight)?
        .name(NAME)?
        .build()
        .await?;
    Ok(conn)
}
