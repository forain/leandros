//! Pi 5 cooling fan on RP1 PWM1 channel 3 (GPIO45), DTB `cooling_fan`
//! (pwm-fan): 41566 ns period, inverted polarity, cooling-levels
//! 0/75/125/175/250 of 255. The tach is measured by RP1 and read back as
//! RPM from PWM1 + 0x3c (`rpm-regmap`/`rpm-offset`).
//!
//! The pi5_board loop calls `tick` once a second with the SoC temperature and
//! the fan follows `rp1_hw::fan_next` (DT trips 50/60/67.5/75 C, 5 C
//! hysteresis, forced top state at 85 C). Until the first reading (and while
//! no temperature is available) it holds `FIXED_STATE`. Knob `fan=auto|off|0..4`.

use crate::rp1_hw as hw;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub const FIXED_STATE: usize = 3;
static BASE: AtomicUsize = AtomicUsize::new(0);
static STATE: AtomicUsize = AtomicUsize::new(usize::MAX);
static RANGE: AtomicUsize = AtomicUsize::new(0);
static CLK: AtomicU64 = AtomicU64::new(0);
/// 0 = auto, 1 = fixed (MODE_STATE holds the state).
static MODE_FIXED: AtomicBool = AtomicBool::new(false);
static MODE_STATE: AtomicUsize = AtomicUsize::new(FIXED_STATE);
static NO_TEMP: AtomicUsize = AtomicUsize::new(0);
static NO_TEMP_LOGGED: AtomicBool = AtomicBool::new(false);

fn log(s: &str) { crate::pci::serial_debug(s) }
fn hex(v: u64) { crate::pci::serial_debug_hex_64(v) }
fn rd(o: usize) -> u32 { unsafe { leandros_lib::mmio::read((BASE.load(Ordering::Relaxed) + o) as *const u32) } }
fn wr(o: usize, v: u32) { unsafe { leandros_lib::mmio::write((BASE.load(Ordering::Relaxed) + o) as *mut u32, v) } }

/// `fan=auto|off|0..4` from the kernel command line (default auto).
pub fn configure(cmdline: &str) {
    let v = cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix("fan="));
    let fixed = match v { Some("off") => Some(0), Some(n) => n.parse::<usize>().ok().filter(|&n| n < hw::FAN_LEVELS.len()), None => None };
    if let Some(s) = fixed { MODE_STATE.store(s, Ordering::Relaxed); MODE_FIXED.store(true, Ordering::Relaxed); }
}

/// Muxes GPIO45 to pwm1, programs channel 3 and starts at `FIXED_STATE`.
pub fn init(bar1: usize) {
    let mut clk = crate::rp1_clk::rate(hw::CLK_PWM1.id);
    if clk == 0 { clk = hw::CLK_PWM1.target_hz; log("[rp1fan] clk_pwm1 rate unknown, assuming 50 MHz\n"); }
    let old = crate::rp1_gpio::function(hw::FAN_PWM_GPIO).unwrap_or(0xff);
    crate::rp1_gpio::set_function(hw::FAN_PWM_GPIO, hw::FSEL_ALT0);
    crate::rp1_gpio::set_pull(hw::FAN_PWM_GPIO, hw::Pull::Down);
    BASE.store(bar1 + hw::PWM1_BASE, Ordering::Release);
    let ch = hw::FAN_PWM_CH;
    let (range, _) = hw::pwm_ticks(clk, hw::FAN_PERIOD_NS, 0);
    RANGE.store(range as usize, Ordering::Relaxed);
    wr(hw::pwm_chan_ctrl(ch), hw::PWM_CHANNEL_DEFAULT | hw::PWM_POLARITY);
    wr(hw::pwm_range(ch), range);
    CLK.store(clk, Ordering::Relaxed);
    let start = if MODE_FIXED.load(Ordering::Relaxed) { MODE_STATE.load(Ordering::Relaxed) } else { FIXED_STATE };
    set_state(start, clk);
    let g = rd(hw::PWM_GLOBAL_CTRL) | hw::pwm_enable_bit(ch);
    wr(hw::PWM_GLOBAL_CTRL, g);
    wr(hw::PWM_GLOBAL_CTRL, g | hw::PWM_SET_UPDATE);
    log("[rp1fan] pwm1 ch3 gpio45 fsel "); hex(old as u64); log("->0 range=");
    hex(range as u64); log(" level="); hex(hw::FAN_LEVELS[start] as u64);
    log(if MODE_FIXED.load(Ordering::Relaxed) { "/255 (fixed)\n" } else { "/255 (auto)\n" });
}

/// Cooling state 0..=4 → duty.
pub fn set_state(s: usize, clk_hz: u64) {
    if BASE.load(Ordering::Acquire) == 0 || s >= hw::FAN_LEVELS.len() { return; }
    if STATE.swap(s, Ordering::Relaxed) == s { return; }
    let (_, duty) = hw::pwm_ticks(clk_hz, hw::FAN_PERIOD_NS, hw::fan_duty_ns(hw::FAN_PERIOD_NS, hw::FAN_LEVELS[s]));
    let ch = hw::FAN_PWM_CH;
    wr(hw::pwm_duty(ch), duty);
    wr(hw::PWM_GLOBAL_CTRL, rd(hw::PWM_GLOBAL_CTRL) | hw::PWM_SET_UPDATE);
}

/// Tach reading (RPM) as RP1 reports it, None before init.
pub fn rpm() -> Option<u32> {
    if BASE.load(Ordering::Acquire) == 0 { None } else { Some(rd(hw::FAN_RPM_OFF)) }
}

/// Current cooling state, None before init.
pub fn state() -> Option<usize> {
    let s = STATE.load(Ordering::Relaxed);
    if BASE.load(Ordering::Acquire) == 0 || s == usize::MAX { None } else { Some(s) }
}
pub fn auto() -> bool { !MODE_FIXED.load(Ordering::Relaxed) }

/// One curve step (board loop, 1 Hz). No-op before init or in fixed mode.
pub fn tick(temp_mc: Option<i32>) {
    let Some(cur) = state() else { return };
    if MODE_FIXED.load(Ordering::Relaxed) { return; }
    let clk = CLK.load(Ordering::Relaxed);
    let Some(t) = temp_mc else {
        if NO_TEMP.fetch_add(1, Ordering::Relaxed) >= 2 {
            set_state(FIXED_STATE, clk);
            if !NO_TEMP_LOGGED.swap(true, Ordering::Relaxed) { log("[rp1fan] no temperature; holding state 3\n"); }
        }
        return;
    };
    NO_TEMP.store(0, Ordering::Relaxed);
    NO_TEMP_LOGGED.store(false, Ordering::Relaxed);
    let next = hw::fan_next(t, cur);
    if next == cur { return; }
    set_state(next, clk);
    log("[rp1fan] "); crate::rp1_clk::dec((t / 1000) as u64); log(".");
    crate::rp1_clk::dec(((t % 1000) / 100) as u64);
    log(" C state "); crate::rp1_clk::dec(cur as u64); log("->"); crate::rp1_clk::dec(next as u64);
    log(" level "); crate::rp1_clk::dec(hw::FAN_LEVELS[next] as u64); log("/255\n");
}
