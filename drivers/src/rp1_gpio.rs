//! RP1 GPIO / pinctrl (`rp1_gpio: gpio@d0000`), after Linux `pinctrl-rp1.c`.
//! Three banks (GPIO 0-27 = the 40-pin header, 28-33, 34-53 board-internal);
//! each pin has an IO CTRL (function select), a PAD (pulls, input enable,
//! output disable) and a bit in its bank's RIO block (software GPIO, with
//! atomic SET/CLR aliases).
//!
//! Kernel API for other drivers (all no-ops returning false/None until the
//! RP1 probe ran): `set_function`, `set_pull`, `output`, `input`, `set`, `get`.

use crate::rp1_hw::{self as hw, Pull};
use core::sync::atomic::{AtomicUsize, Ordering};

static BAR1: AtomicUsize = AtomicUsize::new(0);
/// Serialises read-modify-write of CTRL/PAD (RIO uses SET/CLR aliases).
static LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Called by `rp1::probe` once BAR1 is mapped.
pub fn init(bar1: usize) { BAR1.store(bar1, Ordering::Release); }

fn regs(g: u32) -> Option<(usize, hw::GpioRegs)> {
    let b = BAR1.load(Ordering::Acquire);
    if b == 0 { return None; }
    Some((b, hw::gpio_regs(g)?))
}
fn rd(a: usize) -> u32 { unsafe { leandros_lib::mmio::read(a as *const u32) } }
fn wr(a: usize, v: u32) { unsafe { leandros_lib::mmio::write(a as *mut u32, v) } }

/// Current FUNCSEL (5 = GPIO/RIO, 0x1f = none).
pub fn function(g: u32) -> Option<u32> {
    let (b, r) = regs(g)?;
    Some(rd(b + r.ctrl) & hw::CTRL_FUNCSEL_MASK)
}

/// Selects alt function `fsel` (0..8; `rp1_hw::FSEL_GPIO` for software GPIO)
/// and enables the pad (input on, output-disable off).
pub fn set_function(g: u32, fsel: u32) -> bool {
    let Some((b, r)) = regs(g) else { return false };
    let _l = LOCK.lock();
    wr(b + r.pad, hw::gpio_pad_active(rd(b + r.pad)));
    wr(b + r.ctrl, hw::gpio_ctrl_fsel(rd(b + r.ctrl), fsel));
    true
}

pub fn set_pull(g: u32, pull: Pull) -> bool {
    let Some((b, r)) = regs(g) else { return false };
    let _l = LOCK.lock();
    wr(b + r.pad, hw::gpio_pad_pull(rd(b + r.pad), pull));
    true
}

/// Software GPIO output driving `high`. The level is set before the output
/// is enabled, so the pin never glitches to the other level.
pub fn output(g: u32, high: bool) -> bool {
    let Some((b, r)) = regs(g) else { return false };
    wr(b + r.rio + if high { hw::ALIAS_SET } else { hw::ALIAS_CLR } + hw::RIO_OUT, r.bit);
    wr(b + r.rio + hw::ALIAS_SET + hw::RIO_OE, r.bit);
    set_function(g, hw::FSEL_GPIO)
}

/// Software GPIO input.
pub fn input(g: u32) -> bool {
    let Some((b, r)) = regs(g) else { return false };
    wr(b + r.rio + hw::ALIAS_CLR + hw::RIO_OE, r.bit);
    set_function(g, hw::FSEL_GPIO)
}

/// Drives an output pin (atomic, any context).
pub fn set(g: u32, high: bool) {
    if let Some((b, r)) = regs(g) {
        wr(b + r.rio + if high { hw::ALIAS_SET } else { hw::ALIAS_CLR } + hw::RIO_OUT, r.bit);
    }
}

/// Pin level as the RIO input sees it.
pub fn get(g: u32) -> Option<bool> {
    let (b, r) = regs(g)?;
    Some(rd(b + r.rio + hw::RIO_IN) & r.bit != 0)
}

/// Active-low reset pulse: assert for `ms`, release, wait `ms` (GEM PHY).
pub fn reset_pulse_active_low(g: u32, ms: u64) -> bool {
    if !output(g, true) { return false; }
    set(g, false);
    sched::kthread::msleep(ms);
    set(g, true);
    sched::kthread::msleep(ms);
    true
}
