//! Raspberry Pi 5 board glue: the gpio-leds and gpio-keys DTB nodes.
//! - ACT LED (green): `gio_aon` 9, active low, DTB trigger `mmc0`; lit
//!   while the SD card does I/O (`act_activity`, called by blkdev_sdhci).
//! - PWR LED (red): RP1 GPIO 44, active low, DTB `default-state = "off"`;
//!   set once RP1 is up (`rp1_ready`).
//! - Power button: `gio` 20, active low, KEY_POWER, 50 ms debounce; pushed
//!   into evdev event0 like a keyboard key (the same node QEMU's virtio
//!   keyboard delivers KEY_POWER on), so logind/COSMIC see it.
//! A `pi5-board` kthread polls the button every 50 ms and turns ACT off.
//! `pi5.board=0` on the command line disables all of it.

use crate::rp1_hw as hw;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

static GIO: AtomicUsize = AtomicUsize::new(0);
static GIO_AON: AtomicUsize = AtomicUsize::new(0);
static ACT_ON: AtomicBool = AtomicBool::new(false);
static ACT_LAST_NS: AtomicU64 = AtomicU64::new(0);
static AON_LOCK: AtomicBool = AtomicBool::new(false);
static RP1_UP: AtomicBool = AtomicBool::new(false);
pub static KEY_EVENTS: AtomicU64 = AtomicU64::new(0);

const POLL_MS: u64 = 50;
const ACT_HOLD_NS: u64 = 40_000_000;

fn log(s: &str) { crate::pci::serial_debug(s) }
fn rd(a: usize) -> u32 { unsafe { leandros_lib::mmio::read(a as *const u32) } }
fn wr(a: usize, v: u32) { unsafe { leandros_lib::mmio::write(a as *mut u32, v) } }

/// gio_aon DATA read-modify-write. gio_aon also carries RP1_RUN and the SD
/// card's power/IO-voltage pins, so writers serialise; a contended LED
/// update is simply skipped (it is cosmetic).
fn aon_set(pin: u32, reg: usize, set: bool) -> bool {
    let base = GIO_AON.load(Ordering::Acquire);
    if base == 0 || AON_LOCK.swap(true, Ordering::Acquire) { return false; }
    let (o, bit) = hw::gio_reg(pin, reg);
    let v = rd(base + o);
    wr(base + o, if set { v | bit } else { v & !bit });
    AON_LOCK.store(false, Ordering::Release);
    true
}

fn act(on: bool) {
    // Active low: lit = DATA bit clear.
    if aon_set(hw::ACT_LED_AON_GPIO, hw::GIO_DATA, !on) { ACT_ON.store(on, Ordering::Relaxed); }
}

/// Block I/O happened: light ACT; the board thread turns it off when idle.
#[inline]
pub fn act_activity() {
    if GIO_AON.load(Ordering::Relaxed) == 0 { return; }
    ACT_LAST_NS.store(sched::monotonic_ns(), Ordering::Relaxed);
    if !ACT_ON.load(Ordering::Relaxed) { act(true); }
}

fn button_level_high() -> Option<bool> {
    let base = GIO.load(Ordering::Acquire);
    if base == 0 { return None; }
    let (o, bit) = hw::gio_reg(hw::PWR_KEY_GIO, hw::GIO_DATA);
    Some(rd(base + o) & bit != 0)
}

/// rpi5 build, after blkdev init (kernel init skips it on `pi5.board=0`). Maps gio/gio_aon, ACT off, button input.
pub fn init() {
    let (Some(g), Some(a)) = (
        mm::kva::ioremap(hw::GIO_PHYS, hw::GIO_SIZE, mm::kva::CacheMode::Uncached),
        mm::kva::ioremap(hw::GIO_AON_PHYS, hw::GIO_SIZE, mm::kva::CacheMode::Uncached),
    ) else { log("[pi5] FAIL gio map\n"); return };
    // Button pin: input (the firmware muxes it as GPIO with a pull-up).
    let (o, bit) = hw::gio_reg(hw::PWR_KEY_GIO, hw::GIO_IODIR);
    wr(g + o, rd(g + o) | bit);
    GIO.store(g, Ordering::Release);
    GIO_AON.store(a, Ordering::Release);
    // ACT: off (high), then output.
    act(false);
    aon_set(hw::ACT_LED_AON_GPIO, hw::GIO_IODIR, false);
    let lvl = button_level_high().unwrap_or(true);
    if sched::kthread::spawn("pi5-board", board_thread).is_none() { log("[pi5] FAIL thread\n"); return; }
    log("[pi5] act led gio_aon9, power key gio20 (level ");
    log(if lvl { "high = released)\n" } else { "low = pressed?)\n" });
}

/// Called by `rp1::probe` once RP1 GPIO works: PWR LED to its DTB default (off).
pub fn rp1_ready() {
    crate::rp1_gpio::output(hw::PWR_LED_GPIO, true);
    RP1_UP.store(true, Ordering::Release);
    log("[pi5] pwr led rp1 gpio44 off\n");
}

fn board_thread() {
    let mut key = hw::Debounce::default();
    let mut t = 0u64;
    let mut rpm_logged = false;
    loop {
        sched::kthread::msleep(POLL_MS);
        t += POLL_MS;
        if let Some(high) = button_level_high() {
            if let Some(pressed) = key.feed(high, POLL_MS, hw::PWR_KEY_DEBOUNCE_MS) {
                KEY_EVENTS.fetch_add(1, Ordering::Relaxed);
                evdev_server::push_event(0, 0x01, hw::KEY_POWER, pressed as i32);
                evdev_server::push_event(0, 0x00, 0, 0);
                if pressed { log("[pi5] power key pressed\n"); }
            }
        }
        if ACT_ON.load(Ordering::Relaxed)
            && sched::monotonic_ns().saturating_sub(ACT_LAST_NS.load(Ordering::Relaxed)) > ACT_HOLD_NS {
            act(false);
        }
        if !rpm_logged && t >= 5_000 && RP1_UP.load(Ordering::Acquire) {
            rpm_logged = true;
            if let Some(r) = crate::rp1_fan::rpm() {
                log("[rp1fan] tach ");
                crate::rp1_clk::dec(r as u64);
                log(" rpm\n");
            }
        }
    }
}
