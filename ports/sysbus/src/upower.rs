//! org.freedesktop.UPower, fed from `/sys/class/power_supply`.
//!
//! On a machine whose kernel exports no power_supply class (a desktop PC, QEMU)
//! this reports what upower reports there: no devices, OnBattery = false, a
//! DisplayDevice of Type Unknown with IsPresent = false ("battery-missing"),
//! and a KbdBacklight with maximum 0. On a laptop (the kernel's ACPI battery /
//! AC overlay) it exports `devices/battery_BAT0`, `devices/line_power_AC`, a
//! DisplayDevice aggregating the batteries, OnBattery, and re-reads sysfs every
//! 10 s, emitting PropertiesChanged for what moved.
//!
//! Consumers in the session: cosmic-idle (OnBattery picks AC vs battery idle
//! timeouts), cosmic-osd and cosmic-settings-daemon (KbdBacklight, battery
//! notifications), cosmic-settings' Power page (battery section).

use std::sync::{Arc, Mutex};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

const NAME: &str = "org.freedesktop.UPower";
const PATH: &str = "/org/freedesktop/UPower";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const KBD_BACKLIGHT: &str = "/org/freedesktop/UPower/KbdBacklight";
const SYSFS: &str = "/sys/class/power_supply";

/// Everything the Device interface exposes.
#[derive(Clone, PartialEq)]
struct Dev {
    native_path: String, vendor: String, model: String, serial: String,
    type_: u32, power_supply: bool, online: bool,
    energy: f64, energy_full: f64, energy_full_design: f64, energy_rate: f64,
    voltage: f64, voltage_min_design: f64, charge_cycles: i32,
    time_to_empty: i64, time_to_full: i64, percentage: f64,
    is_present: bool, state: u32, is_rechargeable: bool, capacity: f64,
    technology: u32, warning_level: u32, battery_level: u32, icon_name: String,
}

impl Default for Dev {
    /// What upower exports when nothing in the machine is a battery.
    fn default() -> Self {
        Dev {
            native_path: String::new(), vendor: String::new(), model: String::new(), serial: String::new(),
            type_: 0, power_supply: false, online: false,
            energy: 0.0, energy_full: 0.0, energy_full_design: 0.0, energy_rate: 0.0,
            voltage: 0.0, voltage_min_design: 0.0, charge_cycles: -1,
            time_to_empty: 0, time_to_full: 0, percentage: 0.0,
            is_present: false, state: 0, is_rechargeable: false, capacity: 0.0,
            technology: 0, warning_level: 1, battery_level: 0, icon_name: "battery-missing-symbolic".into(),
        }
    }
}

#[derive(Clone)]
struct Snap {
    /// (object path, device)
    devs: Vec<(String, Dev)>,
    display: Dev,
    on_battery: bool,
}

fn rd(dir: &str, name: &str) -> Option<String> {
    std::fs::read_to_string(format!("{dir}/{name}")).ok().map(|s| s.trim().to_string())
}

fn rdf(dir: &str, name: &str) -> Option<f64> {
    rd(dir, name)?.parse::<f64>().ok()
}

fn battery(dir: &str, name: &str) -> Dev {
    let mut d = Dev { native_path: name.into(), type_: 2, power_supply: true, ..Dev::default() };
    d.is_present = rd(dir, "present").as_deref() == Some("1");
    if !d.is_present { return d; }
    d.vendor = rd(dir, "manufacturer").unwrap_or_default();
    d.model = rd(dir, "model_name").unwrap_or_default();
    d.serial = rd(dir, "serial_number").unwrap_or_default();
    let vmin = rdf(dir, "voltage_min_design").unwrap_or(0.0) / 1e6;
    d.voltage_min_design = vmin;
    d.voltage = rdf(dir, "voltage_now").unwrap_or(0.0) / 1e6;
    // Energy in Wh / rate in W; charge-unit batteries convert with the design voltage.
    let (now, full, design, rate) = if rd(dir, "energy_full").is_some() {
        (rdf(dir, "energy_now"), rdf(dir, "energy_full"), rdf(dir, "energy_full_design"), rdf(dir, "power_now"))
    } else {
        let k = if vmin > 0.0 { vmin } else { 1.0 };
        let c = |v: Option<f64>| v.map(|x| x * k);
        (c(rdf(dir, "charge_now")), c(rdf(dir, "charge_full")), c(rdf(dir, "charge_full_design")), c(rdf(dir, "current_now")))
    };
    d.energy = now.unwrap_or(0.0) / 1e6;
    d.energy_full = full.unwrap_or(0.0) / 1e6;
    d.energy_full_design = design.unwrap_or(0.0) / 1e6;
    d.energy_rate = rate.unwrap_or(0.0) / 1e6;
    d.charge_cycles = rdf(dir, "cycle_count").map(|c| c as i32).unwrap_or(-1);
    d.percentage = rdf(dir, "capacity")
        .unwrap_or(if d.energy_full > 0.0 { d.energy / d.energy_full * 100.0 } else { 0.0 });
    d.capacity = if d.energy_full_design > 0.0 { d.energy_full / d.energy_full_design * 100.0 } else { 0.0 };
    d.is_rechargeable = true;
    d.technology = if rd(dir, "technology").as_deref() == Some("Li-ion") { 1 } else { 0 };
    finish_battery(&mut d, rd(dir, "status").as_deref().unwrap_or("Unknown"));
    d
}

/// State, times, levels and icon from the status text and the numbers in `d`.
fn finish_battery(d: &mut Dev, status: &str) {
    d.state = match status {
        "Charging" => 1,
        "Discharging" => if d.percentage < 1.0 { 3 } else { 2 },
        "Full" => 4,
        "Not charging" => 5,
        _ => 0,
    };
    if d.energy_rate > 0.0 {
        if d.state == 2 { d.time_to_empty = (d.energy / d.energy_rate * 3600.0) as i64; }
        if d.state == 1 { d.time_to_full = ((d.energy_full - d.energy).max(0.0) / d.energy_rate * 3600.0) as i64; }
    }
    d.battery_level = if d.state == 4 { 8 } else if d.percentage <= 5.0 { 4 } else if d.percentage <= 15.0 { 3 } else { 6 };
    d.warning_level = if d.state != 2 { 1 } else if d.percentage <= 5.0 { 4 } else if d.percentage <= 15.0 { 3 } else { 1 };
    let lvl = ((d.percentage / 10.0).round() as u32 * 10).min(100);
    d.icon_name = match d.state {
        4 => "battery-level-100-charged-symbolic".into(),
        1 => format!("battery-level-{lvl}-charging-symbolic"),
        _ => format!("battery-level-{lvl}-symbolic"),
    };
}

/// Read the power_supply class. None when the kernel exports no devices.
fn scan() -> Option<Snap> {
    let mut names: Vec<String> = std::fs::read_dir(SYSFS).ok()?
        .filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    let mut devs = Vec::new();
    let (mut any_ac, mut ac_online) = (false, false);
    for n in &names {
        let dir = format!("{SYSFS}/{n}");
        match rd(&dir, "type").as_deref() {
            Some("Battery") => {
                devs.push((format!("/org/freedesktop/UPower/devices/battery_{n}"), battery(&dir, n)));
            }
            Some("Mains") => {
                any_ac = true;
                let online = rd(&dir, "online").as_deref() == Some("1");
                ac_online |= online;
                devs.push((format!("/org/freedesktop/UPower/devices/line_power_{n}"), Dev {
                    native_path: n.clone(), type_: 1, power_supply: true, online, is_present: true,
                    warning_level: 1, battery_level: 1, icon_name: "ac-adapter-symbolic".into(), ..Dev::default()
                }));
            }
            _ => {}
        }
    }
    let bats: Vec<&Dev> = devs.iter().map(|(_, d)| d).filter(|d| d.type_ == 2 && d.is_present).collect();
    if devs.is_empty() { return None; }
    let mut display = Dev::default();
    if !bats.is_empty() {
        display = Dev { type_: 2, power_supply: true, is_present: true, is_rechargeable: true, ..Dev::default() };
        for b in &bats {
            display.energy += b.energy; display.energy_full += b.energy_full;
            display.energy_full_design += b.energy_full_design; display.energy_rate += b.energy_rate;
            display.voltage = b.voltage; display.technology = b.technology;
        }
        display.percentage = if display.energy_full > 0.0 { display.energy / display.energy_full * 100.0 } else { 0.0 };
        display.capacity = if display.energy_full_design > 0.0 { display.energy_full / display.energy_full_design * 100.0 } else { 0.0 };
        let st = bats.iter().map(|b| b.state).find(|s| *s == 1 || *s == 2).unwrap_or(bats[0].state);
        let status = match st { 1 => "Charging", 2 => "Discharging", 4 => "Full", 5 => "Not charging", _ => "Unknown" };
        finish_battery(&mut display, status);
    }
    let on_battery = !bats.is_empty() && !(any_ac && ac_online);
    Some(Snap { devs, display, on_battery })
}

struct Shared {
    paths: Vec<String>,
    on_battery: Mutex<bool>,
}

struct UPower(Arc<Shared>);

#[zbus::interface(name = "org.freedesktop.UPower")]
impl UPower {
    fn enumerate_devices(&self) -> Vec<OwnedObjectPath> {
        self.0.paths.iter().filter_map(|p| ObjectPath::try_from(p.as_str()).ok().map(Into::into)).collect()
    }

    fn get_display_device(&self) -> OwnedObjectPath {
        ObjectPath::from_static_str_unchecked(DISPLAY_DEVICE).into()
    }

    /// What upower would do at critical battery.
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
        *self.0.on_battery.lock().unwrap()
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

/// One exported device (a battery, an AC adapter, or the DisplayDevice).
struct Device {
    d: Arc<Mutex<Dev>>,
    update_time: Arc<Mutex<u64>>,
}

impl Device {
    fn g(&self) -> Dev { self.d.lock().unwrap().clone() }
}

#[zbus::interface(name = "org.freedesktop.UPower.Device")]
impl Device {
    fn refresh(&self) {}

    fn get_history(&self, _type: &str, _timespan: u32, _resolution: u32) -> Vec<(u32, f64, u32)> {
        Vec::new()
    }

    fn get_statistics(&self, _type: &str) -> Vec<(f64, f64)> {
        Vec::new()
    }

    #[zbus(property)]
    fn native_path(&self) -> String { self.g().native_path }
    #[zbus(property)]
    fn vendor(&self) -> String { self.g().vendor }
    #[zbus(property)]
    fn model(&self) -> String { self.g().model }
    #[zbus(property)]
    fn serial(&self) -> String { self.g().serial }
    #[zbus(property)]
    fn update_time(&self) -> u64 { *self.update_time.lock().unwrap() }
    #[zbus(property, name = "Type")]
    fn type_(&self) -> u32 { self.g().type_ }
    #[zbus(property)]
    fn power_supply(&self) -> bool { self.g().power_supply }
    #[zbus(property)]
    fn has_history(&self) -> bool { false }
    #[zbus(property)]
    fn has_statistics(&self) -> bool { false }
    #[zbus(property)]
    fn online(&self) -> bool { self.g().online }
    #[zbus(property)]
    fn energy(&self) -> f64 { self.g().energy }
    #[zbus(property)]
    fn energy_empty(&self) -> f64 { 0.0 }
    #[zbus(property)]
    fn energy_full(&self) -> f64 { self.g().energy_full }
    #[zbus(property)]
    fn energy_full_design(&self) -> f64 { self.g().energy_full_design }
    #[zbus(property)]
    fn energy_rate(&self) -> f64 { self.g().energy_rate }
    #[zbus(property)]
    fn voltage(&self) -> f64 { self.g().voltage }
    #[zbus(property)]
    fn voltage_min_design(&self) -> f64 { self.g().voltage_min_design }
    #[zbus(property)]
    fn voltage_max_design(&self) -> f64 { 0.0 }
    #[zbus(property)]
    fn charge_cycles(&self) -> i32 { self.g().charge_cycles }
    #[zbus(property)]
    fn luminosity(&self) -> f64 { 0.0 }
    #[zbus(property)]
    fn time_to_empty(&self) -> i64 { self.g().time_to_empty }
    #[zbus(property)]
    fn time_to_full(&self) -> i64 { self.g().time_to_full }
    #[zbus(property)]
    fn percentage(&self) -> f64 { self.g().percentage }
    #[zbus(property)]
    fn temperature(&self) -> f64 { 0.0 }
    #[zbus(property)]
    fn is_present(&self) -> bool { self.g().is_present }
    #[zbus(property)]
    fn state(&self) -> u32 { self.g().state }
    #[zbus(property)]
    fn is_rechargeable(&self) -> bool { self.g().is_rechargeable }
    #[zbus(property)]
    fn capacity(&self) -> f64 { self.g().capacity }
    #[zbus(property)]
    fn technology(&self) -> u32 { self.g().technology }
    #[zbus(property)]
    fn warning_level(&self) -> u32 { self.g().warning_level }
    #[zbus(property)]
    fn battery_level(&self) -> u32 { self.g().battery_level }
    #[zbus(property)]
    fn icon_name(&self) -> String { self.g().icon_name }
    #[zbus(property)]
    fn charge_start_threshold(&self) -> u32 { 0 }
    #[zbus(property)]
    fn charge_end_threshold(&self) -> u32 { 100 }
    #[zbus(property)]
    fn charge_threshold_enabled(&self) -> bool { false }
    #[zbus(property)]
    fn charge_threshold_supported(&self) -> bool { false }
    #[zbus(property)]
    fn charge_threshold_settings_supported(&self) -> u32 { 0 }
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

/// Re-read sysfs; push new values into `state` and emit PropertiesChanged
/// for the properties that moved.
async fn refresh_one(conn: &zbus::Connection, path: &str, state: &Arc<Mutex<Dev>>, ut: &Arc<Mutex<u64>>, new: Dev) {
    let old = state.lock().unwrap().clone();
    if old == new { return; }
    *state.lock().unwrap() = new.clone();
    *ut.lock().unwrap() = crate::now_usec().0 / 1_000_000;
    let Ok(iface) = conn.object_server().interface::<_, Device>(path).await else { return };
    let em = iface.signal_emitter();
    let dev = iface.get().await;
    macro_rules! ch { ($f:ident, $sig:ident) => { if old.$f != new.$f { let _ = dev.$sig(em).await; } }; }
    ch!(percentage, percentage_changed); ch!(state, state_changed); ch!(energy, energy_changed);
    ch!(energy_full, energy_full_changed); ch!(energy_rate, energy_rate_changed);
    ch!(voltage, voltage_changed); ch!(time_to_empty, time_to_empty_changed);
    ch!(time_to_full, time_to_full_changed); ch!(is_present, is_present_changed);
    ch!(online, online_changed); ch!(warning_level, warning_level_changed);
    ch!(battery_level, battery_level_changed); ch!(icon_name, icon_name_changed);
    ch!(capacity, capacity_changed); ch!(charge_cycles, charge_cycles_changed);
    let _ = dev.update_time_changed(em).await;
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    let (update_time, _) = crate::now_usec();
    let ut = update_time / 1_000_000;
    let snap = scan();
    let (devs, display, on_battery) = match &snap {
        Some(s) => (s.devs.clone(), s.display.clone(), s.on_battery),
        None => (Vec::new(), Dev::default(), false),
    };
    let shared = Arc::new(Shared { paths: devs.iter().map(|(p, _)| p.clone()).collect(), on_battery: Mutex::new(on_battery) });
    let mut states: Vec<(String, Arc<Mutex<Dev>>, Arc<Mutex<u64>>)> = Vec::new();
    let mut builder = builder.serve_at(PATH, UPower(shared.clone()))?;
    for (p, d) in std::iter::once((DISPLAY_DEVICE.to_string(), display.clone())).chain(devs.iter().cloned()) {
        let (st, t) = (Arc::new(Mutex::new(d)), Arc::new(Mutex::new(ut)));
        builder = builder.serve_at(ObjectPath::try_from(p.clone())?, Device { d: st.clone(), update_time: t.clone() })?;
        states.push((p, st, t));
    }
    let conn = builder.serve_at(KBD_BACKLIGHT, KbdBacklight)?.name(NAME)?.build().await?;
    if snap.is_some() {
        let c = conn.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let Some(s) = scan() else { continue };
                for (p, st, t) in &states {
                    let new = if p == DISPLAY_DEVICE { Some(s.display.clone()) }
                        else { s.devs.iter().find(|(q, _)| q == p).map(|(_, d)| d.clone()) };
                    if let Some(n) = new { refresh_one(&c, p, st, t, n).await; }
                }
                let changed = std::mem::replace(&mut *shared.on_battery.lock().unwrap(), s.on_battery) != s.on_battery;
                if changed {
                    if let Ok(i) = c.object_server().interface::<_, UPower>(PATH).await {
                        let _ = i.get().await.on_battery_changed(i.signal_emitter()).await;
                    }
                }
            }
        });
    }
    Ok(conn)
}
