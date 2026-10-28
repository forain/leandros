//! Raspberry Pi 5 RP1 clock, GPIO and PWM register math, plus the BCM2712
//! brcmstb GPIO (`gio`, `gio_aon`) bits the board LEDs and power key use.
//! Pure functions, no kernel dependencies; host-tested in drivers/rp1-tests.
//!
//! RP1 offsets are BAR1-relative (DTB address 0xc0_4000_0000 + X = BAR1 + X).
//! Sources: Raspberry Pi Linux 6.12 `clk-rp1.c`, `pinctrl-rp1.c`,
//! `pwm-rp1.c`, `gpio-brcmstb.c`, and the bcm2712d0 Pi 5 DTB.

// ── Clocks (clk-rp1.c) ─────────────────────────────────────────────────────
/// `rp1_clocks: clocks@18000`, 0x10038 bytes.
pub const CLOCKS_BASE: usize = 0x1_8000;
pub const CLOCKS_SIZE: usize = 0x1_0038;
/// RP1 crystal (`clocks = <&clk_xosc>`, 50 MHz).
pub const XOSC_HZ: u64 = 50_000_000;

pub const PLL_CS: usize = 0x00;
pub const PLL_PWR: usize = 0x04;
pub const PLL_FBDIV_INT: usize = 0x08;
pub const PLL_FBDIV_FRAC: usize = 0x0c;
pub const PLL_PRIM: usize = 0x10;
pub const PLL_SEC: usize = 0x14;
pub const PLL_CS_LOCK: u32 = 1 << 31;
pub const PLL_CS_REFDIV_MASK: u32 = 0x3f;
pub const PLL_PWR_PD: u32 = 1 << 0;
pub const PLL_PWR_VCOPD: u32 = 1 << 5;
pub const PLL_PRIM_DIV1_SHIFT: u32 = 16;
pub const PLL_PRIM_DIV2_SHIFT: u32 = 12;
pub const PLL_SEC_DIV_SHIFT: u32 = 8;
pub const PLL_SEC_DIV_MASK: u32 = 0x1f00;
pub const PLL_SEC_RST: u32 = 1 << 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pll { Sys, Audio, Video }

impl Pll {
    /// Offset of the PLL's CS register inside the clocks block.
    pub const fn off(self) -> usize {
        match self { Pll::Sys => 0x8000, Pll::Audio => 0xc000, Pll::Video => 0x1_0000 }
    }
    pub const fn name(self) -> &'static str {
        match self { Pll::Sys => "pll_sys", Pll::Audio => "pll_audio", Pll::Video => "pll_video" }
    }
}

/// Raw PLL registers as read back.
#[derive(Clone, Copy, Default, Debug)]
pub struct PllRegs { pub cs: u32, pub pwr: u32, pub fbdiv_int: u32, pub fbdiv_frac: u32, pub prim: u32, pub sec: u32 }

/// What the firmware left running. `pri` is `pll_<x>` (core / div1 / div2),
/// `pri_ph` its fixed /2 phase output, `sec` the `pll_<x>_sec` divider.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct PllRates { pub core: u64, pub pri: u64, pub pri_ph: u64, pub sec: u64, pub locked: bool, pub powered: bool }

/// VCO = xosc * (FBDIV_INT + FBDIV_FRAC / 2^24) / REFDIV, rounded as Linux
/// `rp1_pll_core_recalc_rate` (which always programs REFDIV = 1).
pub const fn pll_core_hz(r: &PllRegs, xosc: u64) -> u64 {
    let fb = ((r.fbdiv_int as u128) << 24) + (r.fbdiv_frac & 0xff_ffff) as u128;
    let v = ((xosc as u128 * fb + (1 << 23)) >> 24) as u64;
    let refdiv = (r.cs & PLL_CS_REFDIV_MASK) as u64;
    if refdiv > 1 { v / refdiv } else { v }
}

pub const fn pll_rates(r: &PllRegs, xosc: u64) -> PllRates {
    let powered = r.pwr & (PLL_PWR_PD | PLL_PWR_VCOPD) == 0;
    let core = if powered { pll_core_hz(r, xosc) } else { 0 };
    let d1 = ((r.prim >> PLL_PRIM_DIV1_SHIFT) & 7) as u64;
    let d2 = ((r.prim >> PLL_PRIM_DIV2_SHIFT) & 7) as u64;
    let pri = if d1 * d2 == 0 { 0 } else { core / (d1 * d2) };
    let sd = ((r.sec & PLL_SEC_DIV_MASK) >> PLL_SEC_DIV_SHIFT) as u64;
    let sec = if sd == 0 || r.sec & PLL_SEC_RST != 0 { 0 } else { core / sd };
    PllRates { core, pri, pri_ph: pri / 2, sec, locked: r.cs & PLL_CS_LOCK != 0, powered }
}

pub const CLK_CTRL_ENABLE: u32 = 1 << 11;
pub const CLK_CTRL_AUXSRC_SHIFT: u32 = 5;
pub const CLK_CTRL_AUXSRC_MASK: u32 = 0x3e0;
pub const CLK_DIV_FRAC_BITS: u32 = 16;

/// Clock parents this driver can name a rate for. `Other` covers the GP
/// clock sources and anything not modelled (rate unknown, never chosen).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Parent { Xosc, PllSys, PllSysSec, PllVideoSec, Aux, Other }

#[derive(Clone, Copy, Debug)]
pub struct ClkDesc {
    pub name: &'static str,
    /// dt-bindings `rp1.h` index (the cell after `<&rp1_clocks`).
    pub id: u32,
    /// CTRL offset inside the clocks block; DIV_INT = +4, DIV_FRAC = +8, SEL = +0xc.
    pub ctrl: usize,
    pub has_frac: bool,
    pub div_int_max: u32,
    /// Glitchless clocks (clk_sys): SEL/CTRL.SRC picks one of these; `Aux` = use AUXSRC.
    pub std_parents: &'static [Parent],
    pub aux_parents: &'static [Parent],
    pub max_hz: u64,
    /// The DTB `assigned-clock-rates` value.
    pub target_hz: u64,
}

impl ClkDesc {
    pub const fn div_int(&self) -> usize { self.ctrl + 4 }
    pub const fn div_frac(&self) -> usize { self.ctrl + 8 }
    pub const fn sel(&self) -> usize { self.ctrl + 0xc }
}

pub const CLK_SYS: ClkDesc = ClkDesc {
    name: "clk_sys", id: 12, ctrl: 0x14, has_frac: false, div_int_max: 0xff_ffff,
    std_parents: &[Parent::Xosc, Parent::Aux, Parent::PllSys], aux_parents: &[],
    max_hz: 200_000_000, target_hz: 200_000_000,
};
pub const CLK_ETH: ClkDesc = ClkDesc {
    name: "clk_eth", id: 16, ctrl: 0x64, has_frac: false, div_int_max: 0xff,
    std_parents: &[], aux_parents: &[Parent::PllSysSec, Parent::PllSys, Parent::PllVideoSec],
    max_hz: 125_000_000, target_hz: 125_000_000,
};
pub const CLK_PWM1: ClkDesc = ClkDesc {
    name: "clk_pwm1", id: 18, ctrl: 0x84, has_frac: true, div_int_max: 0xffff,
    std_parents: &[], aux_parents: &[Parent::Other, Parent::PllVideoSec, Parent::Xosc],
    max_hz: 76_800_000, target_hz: 50_000_000,
};
pub const CLK_ETH_TSU: ClkDesc = ClkDesc {
    name: "clk_eth_tsu", id: 29, ctrl: 0x134, has_frac: false, div_int_max: 0xff,
    std_parents: &[], aux_parents: &[Parent::Xosc, Parent::PllVideoSec],
    max_hz: 50_000_000, target_hz: 50_000_000,
};

/// Divider value (16.16) from DIV_INT / DIV_FRAC as `rp1_clock_recalc_rate`
/// reads them: DIV_INT 0 means 2^16, DIV_FRAC holds the fraction in its top 16 bits.
pub const fn clk_div_16_16(div_int: u32, div_frac: u32) -> u64 {
    let i = if div_int == 0 { 1u64 << 16 } else { div_int as u64 };
    (i << CLK_DIV_FRAC_BITS) | (div_frac >> (32 - CLK_DIV_FRAC_BITS)) as u64
}

pub const fn clk_rate(parent_hz: u64, div_int: u32, div_frac: u32) -> u64 {
    (parent_hz << CLK_DIV_FRAC_BITS) / clk_div_16_16(div_int, div_frac)
}

/// Divider registers for `rate` from `parent_hz`: (DIV_INT, DIV_FRAC).
/// Integer-only clocks round to the nearest integer. None if out of range.
pub const fn clk_choose_div(parent_hz: u64, rate: u64, has_frac: bool, div_int_max: u32) -> Option<(u32, u32)> {
    if rate == 0 || parent_hz == 0 || rate > parent_hz { return None; }
    let mut d = ((parent_hz << CLK_DIV_FRAC_BITS) + rate / 2) / rate;
    if !has_frac { d = (d + (1 << (CLK_DIV_FRAC_BITS - 1))) & !((1u64 << CLK_DIV_FRAC_BITS) - 1); }
    let i = d >> CLK_DIV_FRAC_BITS;
    if i == 0 || i > div_int_max as u64 { return None; }
    Some((i as u32, ((d & 0xffff) as u32) << (32 - CLK_DIV_FRAC_BITS)))
}

/// Parent a running clock uses: SEL is one-hot over `std_parents` (0 = not
/// yet switched: CTRL.SRC instead); `Aux` resolves through CTRL.AUXSRC.
pub fn clk_parent(d: &ClkDesc, ctrl: u32, sel: u32) -> Parent {
    let aux = || d.aux_parents.get(((ctrl & CLK_CTRL_AUXSRC_MASK) >> CLK_CTRL_AUXSRC_SHIFT) as usize)
        .copied().unwrap_or(Parent::Other);
    if d.std_parents.is_empty() { return aux(); }
    let i = if sel != 0 { sel.trailing_zeros() as usize } else { (ctrl & 3) as usize };
    match d.std_parents.get(i) { Some(Parent::Aux) | None => aux(), Some(p) => *p }
}

/// How to turn on a clock the firmware left off: the first aux parent with a
/// known rate that reaches `target_hz` exactly → (AUXSRC, DIV_INT, DIV_FRAC).
pub fn clk_plan(d: &ClkDesc, rate_of: &dyn Fn(Parent) -> u64) -> Option<(u32, u32, u32)> {
    for (i, p) in d.aux_parents.iter().enumerate() {
        let hz = rate_of(*p);
        if hz == 0 || *p == Parent::Other { continue; }
        if let Some((di, df)) = clk_choose_div(hz, d.target_hz, d.has_frac, d.div_int_max) {
            if clk_rate(hz, di, df) == d.target_hz { return Some((i as u32, di, df)); }
        }
    }
    None
}

/// CTRL value with AUXSRC replaced and ENABLE set.
pub const fn clk_ctrl_enable(ctrl: u32, auxsrc: u32) -> u32 {
    (ctrl & !CLK_CTRL_AUXSRC_MASK) | ((auxsrc << CLK_CTRL_AUXSRC_SHIFT) & CLK_CTRL_AUXSRC_MASK) | CLK_CTRL_ENABLE
}

// ── GPIO / pinctrl (pinctrl-rp1.c) ─────────────────────────────────────────
pub const GPIO_COUNT: u32 = 54;
pub const IO_BASE: usize = 0xd_0000;
pub const RIO_BASE: usize = 0xe_0000;
pub const PADS_BASE: usize = 0xf_0000;
pub const BANK_STRIDE: usize = 0x4000;
pub const ALIAS_XOR: usize = 0x1000;
pub const ALIAS_SET: usize = 0x2000;
pub const ALIAS_CLR: usize = 0x3000;
pub const RIO_OUT: usize = 0x0;
pub const RIO_OE: usize = 0x4;
pub const RIO_IN: usize = 0x8;
pub const CTRL_FUNCSEL_MASK: u32 = 0x1f;
pub const CTRL_OUTOVER_MASK: u32 = 0x3000;
pub const CTRL_OEOVER_MASK: u32 = 0xc000;
pub const FSEL_ALT0: u32 = 0;
pub const FSEL_GPIO: u32 = 5;
pub const FSEL_NONE_HW: u32 = 0x1f;
pub const PAD_SCHMITT: u32 = 1 << 1;
pub const PAD_PULL_SHIFT: u32 = 2;
pub const PAD_PULL_MASK: u32 = 0xc;
pub const PAD_IE: u32 = 1 << 6;
pub const PAD_OD: u32 = 1 << 7;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pull { None = 0, Down = 1, Up = 2 }

/// (bank, pin in bank). Banks hold 28, 6 and 20 pins.
pub const fn gpio_bank(g: u32) -> Option<(usize, u32)> {
    if g < 28 { Some((0, g)) } else if g < 34 { Some((1, g - 28)) } else if g < GPIO_COUNT { Some((2, g - 34)) } else { None }
}

/// Register offsets (BAR1-relative) for GPIO `g`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GpioRegs { pub status: usize, pub ctrl: usize, pub pad: usize, pub rio: usize, pub bit: u32 }

pub const fn gpio_regs(g: u32) -> Option<GpioRegs> {
    let Some((b, p)) = gpio_bank(g) else { return None };
    let io = IO_BASE + b * BANK_STRIDE + 8 * p as usize;
    Some(GpioRegs {
        status: io,
        ctrl: io + 4,
        pad: PADS_BASE + b * BANK_STRIDE + 4 + 4 * p as usize,
        rio: RIO_BASE + b * BANK_STRIDE,
        bit: 1 << p,
    })
}

/// CTRL with FUNCSEL = `fsel` and the output/OE overrides back to "peripheral".
pub const fn gpio_ctrl_fsel(ctrl: u32, fsel: u32) -> u32 {
    (ctrl & !(CTRL_FUNCSEL_MASK | CTRL_OUTOVER_MASK | CTRL_OEOVER_MASK)) | (fsel & CTRL_FUNCSEL_MASK)
}

/// PAD with a function selected: input enabled, open-drain (output disable) off.
pub const fn gpio_pad_active(pad: u32) -> u32 { (pad & !PAD_OD) | PAD_IE }

pub const fn gpio_pad_pull(pad: u32, pull: Pull) -> u32 {
    (pad & !PAD_PULL_MASK) | ((pull as u32) << PAD_PULL_SHIFT)
}

// ── PWM (pwm-rp1.c) ────────────────────────────────────────────────────────
/// `rp1_pwm1: pwm@9c000` (`clocks = <&rp1_clocks RP1_CLK_PWM1>`, 50 MHz).
pub const PWM1_BASE: usize = 0x9_c000;
pub const PWM_SIZE: usize = 0x100;
pub const PWM_GLOBAL_CTRL: usize = 0x0;
pub const fn pwm_chan_ctrl(ch: u32) -> usize { 0x14 + 16 * ch as usize }
pub const fn pwm_range(ch: u32) -> usize { 0x18 + 16 * ch as usize }
pub const fn pwm_duty(ch: u32) -> usize { 0x20 + 16 * ch as usize }
/// FIFO_POP_MASK | trailing-edge mark/space.
pub const PWM_CHANNEL_DEFAULT: u32 = (1 << 8) | 1;
pub const PWM_POLARITY: u32 = 1 << 3;
pub const PWM_SET_UPDATE: u32 = 1 << 31;
pub const fn pwm_enable_bit(ch: u32) -> u32 { 1 << ch }

/// (RANGE, DUTY) register values exactly as `rp1_pwm_apply` computes them.
pub const fn pwm_ticks(clk_hz: u64, period_ns: u64, duty_ns: u64) -> (u32, u32) {
    let clk_period = (1_000_000_000 + clk_hz / 2) / clk_hz;
    (((period_ns + clk_period / 2) / clk_period) as u32, ((duty_ns + clk_period / 2) / clk_period) as u32)
}

/// pwm-fan: duty_ns = DIV_ROUND_UP(level * (period - 1), 255).
pub const fn fan_duty_ns(period_ns: u64, level: u32) -> u64 {
    (level as u64 * (period_ns - 1) + 254) / 255
}

// Fan (DTB `cooling_fan`: pwms = <&rp1_pwm1 3 41566 PWM_POLARITY_INVERTED>,
// cooling-levels, rpm-regmap = <&rp1_pwm1>, rpm-offset = 0x3c).
pub const FAN_PWM_CH: u32 = 3;
pub const FAN_PERIOD_NS: u64 = 41_566;
pub const FAN_LEVELS: [u32; 5] = [0, 75, 125, 175, 250];
pub const FAN_RPM_OFF: usize = 0x3c;
/// `rp1_pwm1_gpio45`: GPIO45 alt0 = pwm1 (channel 3), pull-down.
pub const FAN_PWM_GPIO: u32 = 45;
/// Fan curve over the DTB `cpu-thermal` active trips (50/60/67.5/75 °C,
/// 5 °C hysteresis): millidegrees → cooling level index.
pub const FAN_TRIPS_MC: [i32; 4] = [50_000, 60_000, 67_500, 75_000];
pub const FAN_HYST_MC: i32 = 5_000;
pub fn fan_state(temp_mc: i32, cur: usize) -> usize {
    let mut s = FAN_TRIPS_MC.iter().filter(|&&t| temp_mc >= t).count();
    // Hold the higher state until the temperature drops below trip - hysteresis.
    while s < cur && temp_mc > FAN_TRIPS_MC[s] - FAN_HYST_MC { s += 1; }
    s
}
/// Above this the fan is forced to the top state whatever the hysteresis says.
pub const FAN_CRIT_MC: i32 = 85_000;
/// Curve step used by the board loop: `fan_state` plus the critical override.
pub fn fan_next(temp_mc: i32, cur: usize) -> usize {
    if temp_mc >= FAN_CRIT_MC { FAN_LEVELS.len() - 1 } else { fan_state(temp_mc, cur) }
}

/// `ethernet@100000`: `phy-reset-gpios = <&rp1_gpio 32 GPIO_ACTIVE_LOW>`, 5 ms.
pub const ETH_PHY_RESET_GPIO: u32 = 32;
/// `led-pwr`: `gpios = <&rp1_gpio 44 GPIO_ACTIVE_LOW>` (RP1_STAT_LED).
pub const PWR_LED_GPIO: u32 = 44;

// ── BCM2712 brcmstb GPIO (gpio-brcmstb.c), 32-pin banks 0x20 apart ─────────
/// `gio: gpio@7d508500` (bank widths 32, 4).
pub const GIO_PHYS: u64 = 0x10_7d50_8500;
/// `gio_aon: gpio@7d517c00` (bank widths 15, 6).
pub const GIO_AON_PHYS: u64 = 0x10_7d51_7c00;
pub const GIO_SIZE: usize = 0x40;
pub const GIO_ODEN: usize = 0x00;
pub const GIO_DATA: usize = 0x04;
/// 1 = input.
pub const GIO_IODIR: usize = 0x08;
/// (register offset of bank `reg`, bit) for pin `g`.
pub const fn gio_reg(g: u32, reg: usize) -> (usize, u32) { ((g / 32) as usize * 0x20 + reg, 1 << (g % 32)) }
/// `led-act`: `gpios = <&gio_aon 9 GPIO_ACTIVE_LOW>` (2712_STAT_LED).
pub const ACT_LED_AON_GPIO: u32 = 9;
/// `pwr_button/pwr`: `gpios = <&gio 20 GPIO_ACTIVE_LOW>`, KEY_POWER, 50 ms debounce.
pub const PWR_KEY_GIO: u32 = 20;
pub const KEY_POWER: u16 = 116;
pub const PWR_KEY_DEBOUNCE_MS: u64 = 50;

/// Debounced active-low button: feed raw pin levels every `dt_ms`; returns
/// Some(pressed) when the debounced state changes.
#[derive(Clone, Copy, Default, Debug)]
pub struct Debounce { pub stable: bool, cand: bool, ms: u64 }

impl Debounce {
    pub fn feed(&mut self, level_high: bool, dt_ms: u64, debounce_ms: u64) -> Option<bool> {
        let pressed = !level_high;
        if pressed != self.cand { self.cand = pressed; self.ms = 0; return None; }
        if pressed == self.stable { return None; }
        self.ms += dt_ms;
        if self.ms >= debounce_ms { self.stable = pressed; Some(pressed) } else { None }
    }
}
