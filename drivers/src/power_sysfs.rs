//! Text for `/sys/class/power_supply/*` and `/sys/class/backlight/*`, from the
//! ACPI power module and the i915 PCH backlight, in Linux's names and units.
//! The kernel adapts these plain functions to the VFS `sysclass::Ops` table.
//! `acpi_power.fake=1` on the command line substitutes a fake battery, AC
//! adapter and backlight (for QEMU, which has none).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::acpi_power::{self, BatteryInfo};

static FAKE: AtomicBool = AtomicBool::new(false);
static FAKE_DUTY: AtomicU32 = AtomicU32::new(500);
const FAKE_PERIOD: u32 = 1000;

/// Enable the fake data set (`acpi_power.fake=1`).
pub fn set_fake(on: bool) { FAKE.store(on, Ordering::Relaxed); }
pub fn fake() -> bool { FAKE.load(Ordering::Relaxed) }

/// Module-registered backlight providers (`kapi_backlight_register`), one record per owner.
/// Callbacks are `extern "C"` taking the registrant's `ctx`; the first record that answers wins.
#[derive(Clone, Copy)]
pub struct ModBl {
    pub owner: u32,
    pub get: Option<extern "C" fn(usize, *mut u32, *mut u32) -> bool>,
    pub set: Option<extern "C" fn(usize, u32) -> bool>,
    pub ctx: usize,
}
static MOD_BL: spin::Mutex<Vec<ModBl>> = spin::Mutex::new(Vec::new());
pub fn bl_register(m: ModBl) { MOD_BL.lock().push(m); }
pub fn bl_unregister_owner(owner: u32) { MOD_BL.lock().retain(|m| m.owner != owner); }
pub fn bl_count(owner: u32) -> usize { MOD_BL.lock().iter().filter(|m| m.owner == owner).count() }
/// `(duty, period)` from the first module provider that has a backlight.
pub fn bl_get() -> Option<(u32, u32)> {
    let l: Vec<ModBl> = MOD_BL.lock().clone();
    for m in l {
        if let Some(g) = m.get { let (mut d, mut p) = (0u32, 0u32); if g(m.ctx, &mut d, &mut p) { return Some((d, p)); } }
    }
    None
}
/// Set the duty through the first module provider that accepts it.
pub fn bl_set(duty: u32) -> bool {
    let l: Vec<ModBl> = MOD_BL.lock().clone();
    l.iter().any(|m| m.set.map_or(false, |f| f(m.ctx, duty)))
}

/// Keyboard backlight (`/sys/class/leds/<vendor>::kbd_backlight`): a hardware
/// driver registers `get`/`set` (brightness on the LED's 0..=max scale) with
/// `kbd_register`; `acpi_power.fake=1` serves a fake `hp::kbd_backlight`
/// (max 2: off, low, high) whose level a test moves by writing
/// `brightness_hw_changed`, as the Fn key would.
#[derive(Clone, Copy)]
pub struct KbdLed {
    pub name: &'static str,
    pub max: u32,
    pub get: fn() -> Option<u32>,
    pub set: fn(u32) -> bool,
}
static KBD: spin::Mutex<Option<KbdLed>> = spin::Mutex::new(None);
static FAKE_KBD: AtomicU32 = AtomicU32::new(0);
/// Last level the hardware (not userspace) set; `brightness_hw_changed`.
static KBD_HW: AtomicU32 = AtomicU32::new(0);
pub fn kbd_register(k: KbdLed) { *KBD.lock() = Some(k); }
/// A hardware-initiated change (the Fn key), for `brightness_hw_changed`.
pub fn kbd_hw_changed(level: u32) { KBD_HW.store(level, Ordering::Relaxed); }

fn kbd_led() -> Option<(String, u32, u32)> {
    if let Some(k) = *KBD.lock() { return (k.get)().map(|v| (k.name.to_string(), v, k.max)); }
    if fake() { return Some(("hp::kbd_backlight".to_string(), FAKE_KBD.load(Ordering::Relaxed), 2)); }
    None
}

fn fake_battery() -> BatteryInfo {
    BatteryInfo {
        present: true, status: acpi_power::ST_DISCHARGING, capacity: 76, energy_units: true,
        now: 38_000_000, full: 50_000_000, full_design: 54_000_000, rate: 12_000_000,
        voltage_now: 11_400_000, voltage_min_design: 11_400_000, cycle_count: 123,
        rechargeable: true, model: "FAKE-BAT".to_string(), serial: "0001".to_string(),
        kind: "Li-ion".to_string(), manufacturer: "LeandrOS".to_string(),
    }
}

fn batteries() -> Vec<(usize, BatteryInfo)> {
    if fake() { return alloc::vec![(0, fake_battery())]; }
    (0..acpi_power::battery_count()).filter_map(|i| acpi_power::battery(i).map(|b| (i, b))).collect()
}

fn has_ac() -> bool { fake() || acpi_power::ac_present() }
fn ac_online() -> bool { if fake() { false } else { acpi_power::ac_online().unwrap_or(false) } }

/// Hardware backlight `(duty, period)`; the PCH PWM first, then the fake one.
fn intel_bl() -> Option<(u32, u32)> {
    if let Some(v) = crate::i915::backlight::get() { return Some(v); }
    if fake() { return Some((FAKE_DUTY.load(Ordering::Relaxed), FAKE_PERIOD)); }
    None
}

fn status_str(s: u8) -> &'static str {
    match s {
        acpi_power::ST_CHARGING => "Charging",
        acpi_power::ST_DISCHARGING => "Discharging",
        acpi_power::ST_NOT_CHARGING => "Not charging",
        acpi_power::ST_FULL => "Full",
        _ => "Unknown",
    }
}

fn battery_attrs(b: &BatteryInfo) -> Vec<(&'static str, String)> {
    let mut v: Vec<(&'static str, String)> = Vec::new();
    v.push(("type", "Battery".to_string()));
    // A failed/all-zero _BST/_BIF read must not look like a 0% battery
    // (critical-battery policy would power the machine off): hide it.
    let bogus = b.now == 0 && b.full == 0;
    v.push(("present", ((b.present && !bogus) as u8).to_string()));
    if !b.present { return v; }
    if bogus { v.push(("status", "Unknown".to_string())); return v; }
    v.push(("status", status_str(b.status).to_string()));
    v.push(("capacity", b.capacity.to_string()));
    let level = if b.status == acpi_power::ST_FULL { "Full" }
        else if b.capacity <= 5 { "Critical" } else if b.capacity <= 15 { "Low" } else { "Normal" };
    v.push(("capacity_level", level.to_string()));
    v.push(("technology", if b.rechargeable { "Li-ion" } else { "Unknown" }.to_string()));
    v.push(("cycle_count", b.cycle_count.to_string()));
    v.push(("voltage_now", b.voltage_now.to_string()));
    v.push(("voltage_min_design", b.voltage_min_design.to_string()));
    v.push(("model_name", b.model.clone()));
    v.push(("manufacturer", b.manufacturer.clone()));
    v.push(("serial_number", b.serial.clone()));
    if b.energy_units {
        v.push(("energy_now", b.now.to_string()));
        v.push(("energy_full", b.full.to_string()));
        v.push(("energy_full_design", b.full_design.to_string()));
        v.push(("power_now", b.rate.to_string()));
    } else {
        v.push(("charge_now", b.now.to_string()));
        v.push(("charge_full", b.full.to_string()));
        v.push(("charge_full_design", b.full_design.to_string()));
        v.push(("current_now", b.rate.to_string()));
    }
    v
}

/// The attributes of `class/dev` as `(name, value)` (uevent excluded).
fn attrs_of(class: &str, dev: &str) -> Option<Vec<(&'static str, String)>> {
    match class {
        "power_supply" => {
            if dev == "AC" && has_ac() {
                return Some(alloc::vec![("type", "Mains".to_string()), ("online", (ac_online() as u8).to_string())]);
            }
            let i: usize = dev.strip_prefix("BAT")?.parse().ok()?;
            batteries().into_iter().find(|(n, _)| *n == i).map(|(_, b)| battery_attrs(&b))
        }
        "backlight" => match dev {
            "intel_backlight" => {
                let (duty, period) = intel_bl()?;
                Some(alloc::vec![("brightness", duty.to_string()), ("actual_brightness", duty.to_string()),
                    ("max_brightness", period.to_string()), ("type", "raw".to_string()), ("bl_power", "0".to_string())])
            }
            "acpi_video0" if intel_bl().is_none() => {
                let (cur, max) = acpi_power::acpi_backlight()?;
                Some(alloc::vec![("brightness", cur.to_string()), ("actual_brightness", cur.to_string()),
                    ("max_brightness", max.to_string()), ("type", "firmware".to_string()), ("bl_power", "0".to_string())])
            }
            _ => None,
        },
        "leds" => {
            let (name, cur, max) = kbd_led()?;
            if dev != name { return None; }
            Some(alloc::vec![("brightness", cur.to_string()), ("max_brightness", max.to_string()),
                ("brightness_hw_changed", KBD_HW.load(Ordering::Relaxed).to_string())])
        }
        _ => None,
    }
}

fn dev_names(class: &str) -> Vec<String> {
    let mut v = Vec::new();
    match class {
        "power_supply" => {
            if has_ac() { v.push("AC".to_string()); }
            for (i, _) in batteries() { v.push(format!("BAT{}", i)); }
        }
        "backlight" => {
            if intel_bl().is_some() { v.push("intel_backlight".to_string()); }
            else if acpi_power::acpi_backlight().is_some() { v.push("acpi_video0".to_string()); }
        }
        "leds" => { if let Some((n, _, _)) = kbd_led() { v.push(n); } }
        _ => {}
    }
    v
}

fn put(out: &mut [u8], s: &str) -> usize {
    let n = s.len().min(out.len());
    out[..n].copy_from_slice(&s.as_bytes()[..n]);
    n
}

/// The `idx`-th device name of `class`.
pub fn device(class: &str, idx: usize, out: &mut [u8]) -> Option<usize> {
    dev_names(class).get(idx).map(|n| put(out, n))
}

/// The `idx`-th attribute name of `class/dev` (`uevent` last).
pub fn attr_name(class: &str, dev: &str, idx: usize, out: &mut [u8]) -> Option<usize> {
    let a = attrs_of(class, dev)?;
    if idx < a.len() { Some(put(out, a[idx].0)) } else if idx == a.len() { Some(put(out, "uevent")) } else { None }
}

/// The contents of `class/dev/attr`.
pub fn read(class: &str, dev: &str, attr: &str, out: &mut [u8]) -> Option<usize> {
    let a = attrs_of(class, dev)?;
    if attr == "uevent" {
        let mut s = format!("POWER_SUPPLY_NAME={}\n", dev);
        if class == "backlight" { s.clear(); }
        for (k, v) in &a {
            if class == "power_supply" {
                let k = k.to_ascii_uppercase();
                s += &format!("POWER_SUPPLY_{}={}\n", k, v);
            }
        }
        return Some(put(out, &s));
    }
    let (_, v) = a.iter().find(|(k, _)| *k == attr)?;
    Some(put(out, &format!("{}\n", v)))
}

pub fn writable(class: &str, dev: &str, attr: &str) -> bool {
    (class == "backlight" && attr == "brightness"
        || class == "leds" && (attr == "brightness" || attr == "brightness_hw_changed" && fake() && KBD.lock().is_none()))
        && attrs_of(class, dev).is_some()
}

/// Write `brightness` (decimal text). 0 or a negative errno.
pub fn write(class: &str, dev: &str, attr: &str, data: &[u8]) -> i32 {
    if !writable(class, dev, attr) { return -13; }
    let Ok(t) = core::str::from_utf8(data) else { return -22 };
    let Ok(v) = t.trim().parse::<u32>() else { return -22 };
    if class == "leds" {
        let Some((_, _, max)) = kbd_led() else { return -19 };
        if v > max { return -22; }
        if attr == "brightness_hw_changed" { FAKE_KBD.store(v, Ordering::Relaxed); KBD_HW.store(v, Ordering::Relaxed); return 0; }
        let k = *KBD.lock();
        return match k {
            Some(k) => if (k.set)(v) { 0 } else { -5 },
            None => { FAKE_KBD.store(v, Ordering::Relaxed); 0 }
        };
    }
    let ok = match dev {
        "intel_backlight" => {
            if crate::i915::backlight::get().is_some() { crate::i915::backlight::set(v) }
            else if fake() { FAKE_DUTY.store(v.min(FAKE_PERIOD), Ordering::Relaxed); true }
            else { false }
        }
        _ => acpi_power::acpi_backlight_set(v),
    };
    if ok { 0 } else { -5 }
}

// ── ACPI input device: power/sleep/brightness keys and the lid switch ───────

static INPUT_DEV: AtomicU32 = AtomicU32::new(u32::MAX);

fn input_sink(ty: u16, code: u16, value: i32) {
    let dev = INPUT_DEV.load(Ordering::Acquire);
    if dev == u32::MAX { return; }
    if ty == 5 && code == 0 { evdev_server::set_switch_state((value != 0) as u32); }
    evdev_server::push_event(dev, ty, code, value);
    evdev_server::push_event(dev, 0, 0, 0); // SYN_REPORT
}

/// Register the "ACPI Power Buttons" evdev node if the ACPI module found any
/// power device (or fake data is on) and route its events there. Task context.
/// Returns true when finished (registered, or nothing to register); false to
/// retry later (the evdev server is not up yet).
pub fn register_input() -> bool {
    if INPUT_DEV.load(Ordering::Acquire) != u32::MAX { return true; }
    let lid = acpi_power::lid_open();
    if !(fake() || acpi_power::battery_count() > 0 || acpi_power::ac_present() || lid.is_some()) {
        return true;
    }
    let Some(dev) = evdev_server::register_input_device(evdev_server::DEVKIND_SWITCH,
        "ACPI Power Buttons", 0x19, 0, 0) else { return false };
    evdev_server::set_switch_state(matches!(lid, Some(false)) as u32);
    INPUT_DEV.store(dev, Ordering::Release);
    acpi_power::set_input_sink(input_sink);
    true
}

/// Fake-mode test hook: inject a key/switch event as the ACPI module would.
pub fn inject(ty: u16, code: u16, value: i32) { input_sink(ty, code, value); }
