//! RP1 clock manager (`rp1_clocks: clocks@18000`), after Linux `clk-rp1.c`.
//!
//! The PLLs are taken as the firmware left them (read back, never
//! reprogrammed). The leaf clocks the GEM and the fan need are checked:
//! one already running is left alone and its rate logged; one left off is
//! switched to the first parent that reaches the DTB `assigned-clock-rates`
//! value exactly, then enabled. clk_sys (the GEM pclk/hclk) is only read:
//! RP1's own fabric runs on it.
//!
//! Log: `[rp1clk] pll_sys core=.. MHz pri=.. sec=.. lock=1`, then one line
//! per clock `[rp1clk] clk_eth 125000000 Hz (pll_sys_sec, was on)`.

use crate::rp1_hw::{self as hw, ClkDesc, Parent, Pll, PllRates, PllRegs};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static BASE: AtomicUsize = AtomicUsize::new(0);
static RATES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
const CLKS: [ClkDesc; 4] = [hw::CLK_SYS, hw::CLK_ETH, hw::CLK_ETH_TSU, hw::CLK_PWM1];

fn log(s: &str) { crate::pci::serial_debug(s) }
pub fn dec(v: u64) {
    let mut b = [0u8; 20];
    let mut i = b.len();
    let mut v = v;
    loop { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    log(core::str::from_utf8(&b[i..]).unwrap_or("?"));
}

fn rd(o: usize) -> u32 { unsafe { leandros_lib::mmio::read((BASE.load(Ordering::Relaxed) + o) as *const u32) } }
fn wr(o: usize, v: u32) { unsafe { leandros_lib::mmio::write((BASE.load(Ordering::Relaxed) + o) as *mut u32, v) } }

fn pll(p: Pll) -> PllRates {
    let o = p.off();
    let r = PllRegs {
        cs: rd(o + hw::PLL_CS), pwr: rd(o + hw::PLL_PWR), fbdiv_int: rd(o + hw::PLL_FBDIV_INT),
        fbdiv_frac: rd(o + hw::PLL_FBDIV_FRAC), prim: rd(o + hw::PLL_PRIM), sec: rd(o + hw::PLL_SEC),
    };
    hw::pll_rates(&r, hw::XOSC_HZ)
}

fn parent_name(p: Parent) -> &'static str {
    match p {
        Parent::Xosc => "xosc", Parent::PllSys => "pll_sys", Parent::PllSysSec => "pll_sys_sec",
        Parent::PllVideoSec => "pll_video_sec", Parent::Aux | Parent::Other => "other",
    }
}

/// Rate of RP1 clock `id` (dt-bindings index) as last read, 0 if unknown.
pub fn rate(id: u32) -> u64 {
    CLKS.iter().position(|c| c.id == id).map_or(0, |i| RATES[i].load(Ordering::Relaxed))
}

/// Logs the PLLs and makes sure clk_eth, clk_eth_tsu and clk_pwm1 run.
/// `bar1` = RP1 BAR1 VA. Returns false if a GEM clock could not be enabled.
pub fn init(bar1: usize) -> bool {
    BASE.store(bar1 + hw::CLOCKS_BASE, Ordering::Relaxed);
    let sys = pll(Pll::Sys);
    let video = pll(Pll::Video);
    for (p, r) in [(Pll::Sys, sys), (Pll::Audio, pll(Pll::Audio)), (Pll::Video, video)] {
        log("[rp1clk] "); log(p.name());
        log(" core="); dec(r.core / 1_000_000);
        log(" MHz pri="); dec(r.pri / 1_000_000);
        log(" sec="); dec(r.sec / 1_000_000);
        log(" lock="); dec(r.locked as u64);
        log("\n");
    }
    let rate_of = |p: Parent| -> u64 {
        match p {
            Parent::Xosc => hw::XOSC_HZ, Parent::PllSys => sys.pri, Parent::PllSysSec => sys.sec,
            Parent::PllVideoSec => video.sec, Parent::Aux | Parent::Other => 0,
        }
    };
    let mut ok = true;
    for (i, d) in CLKS.iter().enumerate() {
        let ctrl = rd(d.ctrl);
        let mut how = "was on";
        if ctrl & hw::CLK_CTRL_ENABLE == 0 && d.std_parents.is_empty() {
            match hw::clk_plan(d, &rate_of) {
                Some((aux, di, df)) => {
                    wr(d.div_int(), di);
                    if d.has_frac { wr(d.div_frac(), df); }
                    wr(d.ctrl, hw::clk_ctrl_enable(ctrl, aux));
                    how = "enabled";
                }
                None => { how = "OFF, no parent"; if d.id != hw::CLK_PWM1.id { ok = false; } }
            }
        }
        let ctrl = rd(d.ctrl);
        let p = hw::clk_parent(d, ctrl, if d.std_parents.is_empty() { 0 } else { rd(d.sel()) });
        let hz = if ctrl & hw::CLK_CTRL_ENABLE == 0 && d.std_parents.is_empty() { 0 } else {
            hw::clk_rate(rate_of(p), rd(d.div_int()), if d.has_frac { rd(d.div_frac()) } else { 0 })
        };
        RATES[i].store(hz, Ordering::Relaxed);
        log("[rp1clk] "); log(d.name); log(" "); dec(hz);
        log(" Hz ("); log(parent_name(p)); log(", "); log(how); log(")\n");
    }
    ok
}
