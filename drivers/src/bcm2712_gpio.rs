//! BCM2712 GPIO, pin mux and level-2 interrupt controllers (Raspberry Pi 5).
//!
//! * GPIO: the two brcmstb GPIO blocks, `gio` @ 0x7d508500 (WL_ON = 28,
//!   BT_ON = 29, BT uart 24..27, Wi-Fi SDIO 30..35) and `gio_aon` @ 0x7d517c00
//!   (RP1_RUN, SD power, status LED, PMIC_INT). Register layout per Linux
//!   `gpio-brcmstb.c`: one 0x20-byte bank per 32 lines (ODEN, DATA, IODIR,
//!   EC, EI, MASK, LEVEL, STAT). Bank widths differ by stepping (dts
//!   `brcm,gpio-bank-widths`): C0 gio 32+22 / aon 17+6, D0 gio 32+4 / aon 15+6.
//! * Pin mux: `pinctrl` @ 0x7d504100 and `pinctrl_aon` @ 0x7d510700, Linux
//!   `pinctrl-bcm2712.c`. The C0 and D0 layouts differ; the tables in
//!   [`tables`] are generated from the Linux source and the stepping comes
//!   from the chip id ([`crate::bcm2712_id`]), never from the live DT (the
//!   firmware can hand a D0 chip a C1 tree).
//! * Level-2 interrupt controllers (Linux `irq-brcmstb-l2.c`), chained on GIC
//!   SPIs. In the D0 tree: `main_irq` (bcm7271, SPI 244) carries `gio` at
//!   hwirq 0; `bsc_irq` (bcm7271, SPI 242) carries the HDMI DDC i2c (ddc0/1);
//!   `cpu_l2_irq` (brcm,l2-intc edge, SPI 238) has no consumer in the tree.
//!   `aon_intr`, `main_aon_irq` and the l2 @ 0x7d517000 are disabled there.
//!   The BT uart (`uarta`, SPI 276) is a direct GIC line, not behind an l2.
//!
//! The register logic is generic over [`Regs`] so the host tests drive it
//! against a mock (drivers/bcm2712-tests). The MMIO half and the kernel API
//! (`gpio_*`, `pin_*`, `l2_request`, `gpio_request_irq`) are `rpi5`-only.

#[path = "bcm2712_pinctrl_tables.rs"]
pub mod tables;

use crate::bcm2712_id::Stepping;
use tables::{Pin, FUNC_NAMES, NONE, NO_FUNC};

/// 32-bit register window.
pub trait Regs {
    fn rd(&mut self, off: usize) -> u32;
    fn wr(&mut self, off: usize, v: u32);
}

// ── GPIO (brcmstb) ───────────────────────────────────────────────────────────

pub const GIO_BANK_SIZE: usize = 0x20;
pub const GIO_ODEN: usize = 0x00;
pub const GIO_DATA: usize = 0x04;
/// 1 = input.
pub const GIO_IODIR: usize = 0x08;
/// Edge config: 1 = rising / active high.
pub const GIO_EC: usize = 0x0c;
/// Edge insensitive: 1 = both edges.
pub const GIO_EI: usize = 0x10;
/// 1 = interrupt enabled.
pub const GIO_MASK: usize = 0x14;
/// 1 = level triggered.
pub const GIO_LEVEL: usize = 0x18;
/// Latched status, write 1 to clear.
pub const GIO_STAT: usize = 0x1c;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chip { Gio, GioAon }

/// Lines on `gio`: Wi-Fi power, Bluetooth power.
pub const WL_ON: u32 = 28;
pub const BT_ON: u32 = 29;

/// Bank widths of a GPIO block for a stepping. An unknown stepping gets the
/// narrower D0 layout (a subset of C0).
pub fn bank_widths(chip: Chip, st: Stepping) -> &'static [u32] {
    match (chip, st) {
        (Chip::Gio, Stepping::C0) => &[32, 22],
        (Chip::Gio, _) => &[32, 4],
        (Chip::GioAon, Stepping::C0) => &[17, 6],
        (Chip::GioAon, _) => &[15, 6],
    }
}

pub fn line_count(widths: &[u32]) -> u32 { widths.iter().sum() }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GpioError { BadLine, Unavailable, NoIrq, Busy }

/// (bank register base, bit) of a line; lines are numbered across banks.
pub fn locate(widths: &[u32], line: u32) -> Result<(usize, u32), GpioError> {
    let mut base = 0u32;
    for (b, &w) in widths.iter().enumerate() {
        if line < base + w { return Ok((b * GIO_BANK_SIZE, line - base)); }
        base += w;
    }
    Err(GpioError::BadLine)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trigger { Rising, Falling, Both, High, Low }

/// (LEVEL, EC, EI) bits for a trigger, as `brcmstb_gpio_irq_set_type`.
pub fn trigger_bits(t: Trigger) -> (bool, bool, bool) {
    match t {
        Trigger::Rising => (false, true, false),
        Trigger::Falling => (false, false, false),
        Trigger::Both => (false, false, true),
        Trigger::High => (true, true, false),
        Trigger::Low => (true, false, false),
    }
}

pub struct Gpio<R: Regs> {
    pub regs: R,
    pub widths: &'static [u32],
}

impl<R: Regs> Gpio<R> {
    pub fn new(regs: R, widths: &'static [u32]) -> Self { Gpio { regs, widths } }

    fn rmw(&mut self, off: usize, bit: u32, set: bool) -> bool {
        let v = self.regs.rd(off);
        let n = if set { v | (1 << bit) } else { v & !(1 << bit) };
        if n != v { self.regs.wr(off, n); }
        n != v
    }

    /// Drive a line: DATA first (read-modify-write, always written), then
    /// IODIR cleared if the line was an input, so it never glitches to the
    /// old level.
    pub fn set_output(&mut self, line: u32, value: bool) -> Result<(), GpioError> {
        let (b, bit) = locate(self.widths, line)?;
        let d = self.regs.rd(b + GIO_DATA);
        self.regs.wr(b + GIO_DATA, if value { d | (1 << bit) } else { d & !(1 << bit) });
        let dir = self.regs.rd(b + GIO_IODIR);
        if dir & (1 << bit) != 0 { self.regs.wr(b + GIO_IODIR, dir & !(1 << bit)); }
        Ok(())
    }

    pub fn set_input(&mut self, line: u32) -> Result<(), GpioError> {
        let (b, bit) = locate(self.widths, line)?;
        self.rmw(b + GIO_IODIR, bit, true);
        Ok(())
    }

    pub fn get(&mut self, line: u32) -> Result<bool, GpioError> {
        let (b, bit) = locate(self.widths, line)?;
        Ok(self.regs.rd(b + GIO_DATA) & (1 << bit) != 0)
    }

    pub fn is_output(&mut self, line: u32) -> Result<bool, GpioError> {
        let (b, bit) = locate(self.widths, line)?;
        Ok(self.regs.rd(b + GIO_IODIR) & (1 << bit) == 0)
    }

    pub fn set_trigger(&mut self, line: u32, t: Trigger) -> Result<(), GpioError> {
        let (b, bit) = locate(self.widths, line)?;
        let (lvl, ec, ei) = trigger_bits(t);
        self.rmw(b + GIO_LEVEL, bit, lvl);
        self.rmw(b + GIO_EC, bit, ec);
        self.rmw(b + GIO_EI, bit, ei);
        Ok(())
    }

    pub fn set_irq_enabled(&mut self, line: u32, on: bool) -> Result<(), GpioError> {
        let (b, bit) = locate(self.widths, line)?;
        self.rmw(b + GIO_MASK, bit, on);
        Ok(())
    }

    pub fn ack(&mut self, line: u32) -> Result<(), GpioError> {
        let (b, bit) = locate(self.widths, line)?;
        self.regs.wr(b + GIO_STAT, 1 << bit);
        Ok(())
    }

    /// Mask every line and clear latched status (the probe-time reset Linux does).
    pub fn irq_reset(&mut self) {
        for b in 0..self.widths.len() {
            self.regs.wr(b * GIO_BANK_SIZE + GIO_MASK, 0);
            self.regs.wr(b * GIO_BANK_SIZE + GIO_STAT, 0xFFFF_FFFF);
        }
    }

    /// Chained handler body: for every enabled, pending line, ack it (W1C)
    /// then call `f(line)`. Returns the number of lines dispatched.
    pub fn dispatch(&mut self, mut f: impl FnMut(u32)) -> u32 {
        let mut n = 0;
        let mut base = 0;
        for (b, &w) in self.widths.iter().enumerate() {
            let off = b * GIO_BANK_SIZE;
            let wmask = if w >= 32 { u32::MAX } else { (1u32 << w) - 1 };
            let mut pend = self.regs.rd(off + GIO_STAT) & self.regs.rd(off + GIO_MASK) & wmask;
            while pend != 0 {
                let bit = pend.trailing_zeros();
                pend &= pend - 1;
                self.regs.wr(off + GIO_STAT, 1 << bit);
                f(base + bit);
                n += 1;
            }
            base += w;
        }
        n
    }
}

// ── Pin mux (pinctrl-bcm2712) ────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Block { Main, Aon }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pull { None = 0, Down = 1, Up = 2 }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinError { UnknownStepping, NoPin, NoFunction, NoMux, NoPad, Unavailable }

pub const FSEL_COUNT: u32 = 9;

pub fn table(block: Block, st: Stepping) -> Option<&'static [Pin]> {
    match (block, st) {
        (Block::Main, Stepping::C0) => Some(&tables::C0_MAIN),
        (Block::Aon, Stepping::C0) => Some(&tables::C0_AON),
        (Block::Main, Stepping::D0) => Some(&tables::D0_MAIN),
        (Block::Aon, Stepping::D0) => Some(&tables::D0_AON),
        (_, Stepping::Unknown) => None,
    }
}

/// Bytes of register space a block's table reaches (mux and pad registers).
pub fn table_span(tab: &[Pin]) -> usize {
    tab.iter().flat_map(|p| [p.mux, p.pad]).filter(|&b| b != NONE)
        .map(|b| (b as usize / 32) * 4 + 4).max().unwrap_or(0)
}

pub fn func_id(name: &str) -> Option<u8> { FUNC_NAMES.iter().position(|&n| n == name).map(|i| i as u8) }

pub fn pin(tab: &'static [Pin], num: u32) -> Option<&'static Pin> { tab.iter().find(|p| p.num as u32 == num) }

/// (register byte offset, shift) of a table bit offset.
pub fn bit_reg(bit: u16) -> (usize, u32) { ((bit as usize >> 5) << 2, bit as u32 & 31) }

/// fsel value selecting `func` on `p`, as `bcm2712_pinctrl_fsel_set`: ids
/// 0..=8 (gpio, alt1..alt8) are raw fsel numbers, others are searched.
pub fn fsel_for(p: &Pin, func: u8) -> Option<u32> {
    if (func as u32) < FSEL_COUNT { return Some(func as u32); }
    if func == NO_FUNC { return None; }
    p.funcs.iter().position(|&f| f == func).map(|i| i as u32 + 1)
}

/// One pin's setting in a group: function name, and the pull to program
/// (`None` leaves the pad alone, like a dts group without a bias property).
#[derive(Clone, Copy, Debug)]
pub struct PinConf { pub pin: u32, pub func: &'static str, pub pull: Option<Pull> }

const fn pc(pin: u32, func: &'static str, pull: Option<Pull>) -> PinConf { PinConf { pin, func, pull } }

/// Wi-Fi: WL_ON as GPIO (pad pulls off) and sdio2 on 30..35 (dts `wl_on_pins`
/// + `sdio2_30_pins`). Order matters for `writes` accounting only.
pub const GROUP_SDIO2_WL_ON: [PinConf; 7] = [
    pc(28, "gpio", Some(Pull::None)),
    pc(30, "sd2", Some(Pull::None)),
    pc(31, "sd2", Some(Pull::Up)),
    pc(32, "sd2", Some(Pull::Up)),
    pc(33, "sd2", Some(Pull::Up)),
    pc(34, "sd2", Some(Pull::Up)),
    pc(35, "sd2", Some(Pull::Up)),
];
/// Bluetooth uart (`uarta_24_pins`): RTS, CTS (up), TXD, RXD (up).
pub const GROUP_UARTA: [PinConf; 4] = [
    pc(24, "uart0", Some(Pull::None)),
    pc(25, "uart0", Some(Pull::Up)),
    pc(26, "uart0", Some(Pull::None)),
    pc(27, "uart0", Some(Pull::Up)),
];
/// BT_ON as GPIO (`bt_shutdown_pins`, no bias).
pub const GROUP_BT_SHUTDOWN: [PinConf; 1] = [pc(29, "gpio", None)];

pub struct Pinctrl<R: Regs> {
    pub regs: R,
    pub tab: &'static [Pin],
}

impl<R: Regs> Pinctrl<R> {
    pub fn new(regs: R, tab: &'static [Pin]) -> Self { Pinctrl { regs, tab } }

    pub fn get_fsel(&mut self, num: u32) -> Option<u32> {
        let p = pin(self.tab, num)?;
        if p.mux == NONE { return Some(0); }
        let (r, s) = bit_reg(p.mux);
        Some((self.regs.rd(r) >> s) & 0xF)
    }

    /// Name of the function currently selected ("gpio", "sd2", or "altN" when the
    /// fsel has no named function on this pin).
    pub fn get_function(&mut self, num: u32) -> Option<&'static str> {
        let fsel = self.get_fsel(num)?;
        if fsel == 0 { return Some("gpio"); }
        let p = pin(self.tab, num)?;
        let f = p.funcs.get(fsel as usize - 1).copied().unwrap_or(NO_FUNC);
        Some(if f == NO_FUNC { FUNC_NAMES[fsel.min(8) as usize] } else { FUNC_NAMES[f as usize] })
    }

    /// Select `func` on pin `num`. Writes only on change; Ok(true) = written.
    pub fn set_function(&mut self, num: u32, func: &str) -> Result<bool, PinError> {
        let p = pin(self.tab, num).ok_or(PinError::NoPin)?;
        let fid = func_id(func).ok_or(PinError::NoFunction)?;
        let fsel = fsel_for(p, fid).ok_or(PinError::NoFunction)?;
        if p.mux == NONE { return if fsel == 0 { Ok(false) } else { Err(PinError::NoMux) }; }
        let (r, s) = bit_reg(p.mux);
        let v = self.regs.rd(r);
        if (v >> s) & 0xF == fsel { return Ok(false); }
        self.regs.wr(r, (v & !(0xF << s)) | (fsel << s));
        Ok(true)
    }

    pub fn get_pull(&mut self, num: u32) -> Option<u32> {
        let p = pin(self.tab, num)?;
        if p.pad == NONE { return None; }
        let (r, s) = bit_reg(p.pad);
        Some((self.regs.rd(r) >> s) & 3)
    }

    pub fn set_pull(&mut self, num: u32, pull: Pull) -> Result<bool, PinError> {
        let p = pin(self.tab, num).ok_or(PinError::NoPin)?;
        if p.pad == NONE { return Err(PinError::NoPad); }
        let (r, s) = bit_reg(p.pad);
        let v = self.regs.rd(r);
        if (v >> s) & 3 == pull as u32 { return Ok(false); }
        self.regs.wr(r, (v & !(3 << s)) | ((pull as u32) << s));
        Ok(true)
    }

    /// Apply a group pin by pin (function, then pull). Returns the register
    /// writes made; stops at the first error.
    pub fn apply(&mut self, group: &[PinConf]) -> Result<u32, PinError> {
        let mut writes = 0;
        for c in group {
            writes += self.set_function(c.pin, c.func)? as u32;
            if let Some(p) = c.pull { writes += self.set_pull(c.pin, p)? as u32; }
        }
        Ok(writes)
    }
}

// ── Level-2 interrupt controllers (irq-brcmstb-l2) ───────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum L2Kind {
    /// `brcm,l2-intc`: edge, latched, cleared through CPU_CLEAR.
    Edge,
    /// `brcm,bcm7271-l2-intc`: level, 0x10 bytes, no clear register.
    Level7271,
    /// `brcm,bcm2711-l2-intc`: level with the edge layout.
    Level2711,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct L2Offs { pub status: usize, pub clear: Option<usize>, pub mask_status: usize, pub mask_set: usize, pub mask_clear: usize }

pub const fn l2_offs(k: L2Kind) -> L2Offs {
    match k {
        L2Kind::Edge => L2Offs { status: 0x00, clear: Some(0x08), mask_status: 0x0c, mask_set: 0x10, mask_clear: 0x14 },
        L2Kind::Level7271 => L2Offs { status: 0x00, clear: None, mask_status: 0x04, mask_set: 0x08, mask_clear: 0x0c },
        L2Kind::Level2711 => L2Offs { status: 0x00, clear: Some(0x08), mask_status: 0x0c, mask_set: 0x10, mask_clear: 0x14 },
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum L2Id { MainIrq = 0, BscIrq = 1, CpuL2 = 2 }

#[derive(Clone, Copy, Debug)]
pub struct L2Desc { pub name: &'static str, pub phys: u64, pub spi: u32, pub kind: L2Kind, pub consumers: &'static str }

/// The enabled l2 controllers of the D0 tree (same SPIs on C1). Physical
/// addresses include the soc `ranges` offset (+0x10_0000_0000).
pub const L2_CONTROLLERS: [L2Desc; 3] = [
    L2Desc { name: "main_irq", phys: 0x10_7d50_8400, spi: 0xf4, kind: L2Kind::Level7271, consumers: "gio@0" },
    L2Desc { name: "bsc_irq", phys: 0x10_7d50_8380, spi: 0xf2, kind: L2Kind::Level7271, consumers: "ddc0@0,ddc1@1" },
    L2Desc { name: "cpu_l2_irq", phys: 0x10_7d50_3000, spi: 0xee, kind: L2Kind::Edge, consumers: "-" },
];

/// GIC INTID of an SPI.
pub const fn spi_intid(spi: u32) -> u32 { spi + 32 }

pub struct L2<R: Regs> { pub regs: R, pub offs: L2Offs }

impl<R: Regs> L2<R> {
    pub fn new(regs: R, kind: L2Kind) -> Self { L2 { regs, offs: l2_offs(kind) } }
    /// Mask everything (and clear latched edges), as the Linux init does.
    pub fn reset(&mut self) {
        self.regs.wr(self.offs.mask_set, 0xFFFF_FFFF);
        if let Some(c) = self.offs.clear { self.regs.wr(c, 0xFFFF_FFFF); }
    }
    pub fn mask(&mut self, hw: u32) { self.regs.wr(self.offs.mask_set, 1 << hw); }
    pub fn unmask(&mut self, hw: u32) { self.regs.wr(self.offs.mask_clear, 1 << hw); }
    pub fn pending(&mut self) -> u32 { self.regs.rd(self.offs.status) & !self.regs.rd(self.offs.mask_status) }
    /// Chained handler body: for each pending source, ack (edge kinds) and
    /// call `f(hwirq)`. A source nobody handles is masked so it cannot storm.
    /// Returns (dispatched, newly masked).
    pub fn dispatch(&mut self, mut f: impl FnMut(u32) -> bool) -> (u32, u32) {
        let mut pend = self.pending();
        let (mut n, mut masked) = (0, 0);
        while pend != 0 {
            let hw = pend.trailing_zeros();
            pend &= pend - 1;
            if let Some(c) = self.offs.clear { self.regs.wr(c, 1 << hw); }
            if f(hw) { n += 1; } else { self.mask(hw); masked |= 1 << hw; }
        }
        (n, masked)
    }
}

// ── Kernel half (Raspberry Pi 5) ─────────────────────────────────────────────

#[cfg(feature = "rpi5")]
pub use hw::*;

#[cfg(feature = "rpi5")]
mod hw {
    extern crate alloc;
    use super::*;
    use crate::irq::{IrqHandler, IrqReturn, IrqSource};
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use leandros_lib::mmio::{MmioRd, MmioWr};
    use spin::Mutex;

    pub const GIO_PHYS: u64 = 0x10_7d50_8500;
    pub const GIO_AON_PHYS: u64 = 0x10_7d51_7c00;
    pub const PINCTRL_PHYS: u64 = 0x10_7d50_4100;
    pub const PINCTRL_AON_PHYS: u64 = 0x10_7d51_0700;

    pub struct Mmio(pub usize);
    impl Regs for Mmio {
        fn rd(&mut self, off: usize) -> u32 { unsafe { ((self.0 + off) as *const u32).mmio_read() } }
        fn wr(&mut self, off: usize, v: u32) { unsafe { ((self.0 + off) as *mut u32).mmio_write(v) } }
    }

    // gio, gio_aon, pinctrl, pinctrl_aon, then the l2 controllers.
    static BASE: [AtomicUsize; 4 + L2_CONTROLLERS.len()] = [const { AtomicUsize::new(0) }; 4 + L2_CONTROLLERS.len()];
    static MAP_LOCK: Mutex<()> = Mutex::new(());
    /// Serialises read-modify-write from thread context. IRQ handlers only
    /// write W1C/set/clear registers and never take it.
    static RMW: Mutex<()> = Mutex::new(());
    /// Set when the boot probe saw an SError: every call then fails.
    static DEAD: AtomicBool = AtomicBool::new(false);

    fn map_one(i: usize, phys: u64, len: usize) -> Option<usize> {
        let b = BASE[i].load(Ordering::Acquire);
        if b != 0 { return Some(b); }
        let _g = MAP_LOCK.lock();
        let b = BASE[i].load(Ordering::Acquire);
        if b != 0 { return Some(b); }
        let v = mm::kva::ioremap(phys, len, mm::kva::CacheMode::Uncached)?;
        BASE[i].store(v, Ordering::Release);
        Some(v)
    }

    pub fn stepping() -> Stepping { crate::bcm2712_id::stepping().stepping }

    fn gpio(chip: Chip) -> Result<Gpio<Mmio>, GpioError> {
        if DEAD.load(Ordering::Relaxed) { return Err(GpioError::Unavailable); }
        let (i, phys) = match chip { Chip::Gio => (0, GIO_PHYS), Chip::GioAon => (1, GIO_AON_PHYS) };
        let b = map_one(i, phys, 0x40).ok_or(GpioError::Unavailable)?;
        Ok(Gpio::new(Mmio(b), bank_widths(chip, stepping())))
    }

    fn pinctrl(block: Block) -> Result<Pinctrl<Mmio>, PinError> {
        if DEAD.load(Ordering::Relaxed) { return Err(PinError::Unavailable); }
        let tab = table(block, stepping()).ok_or(PinError::UnknownStepping)?;
        let (i, phys) = match block { Block::Main => (2, PINCTRL_PHYS), Block::Aon => (3, PINCTRL_AON_PHYS) };
        // C0 main reaches 0x30, D0 main 0x1c; map the larger once.
        let b = map_one(i, phys, 0x30).ok_or(PinError::Unavailable)?;
        Ok(Pinctrl::new(Mmio(b), tab))
    }

    /// Drive a GPIO line (output). BT_ON = `gpio_set_output(Chip::Gio, BT_ON, true)`.
    pub fn gpio_set_output(chip: Chip, line: u32, value: bool) -> Result<(), GpioError> {
        let mut g = gpio(chip)?;
        let _l = RMW.lock();
        g.set_output(line, value)
    }
    pub fn gpio_set_input(chip: Chip, line: u32) -> Result<(), GpioError> {
        let mut g = gpio(chip)?;
        let _l = RMW.lock();
        g.set_input(line)
    }
    pub fn gpio_get(chip: Chip, line: u32) -> Result<bool, GpioError> { gpio(chip)?.get(line) }
    pub fn gpio_is_output(chip: Chip, line: u32) -> Result<bool, GpioError> { gpio(chip)?.is_output(line) }

    pub fn pin_set_function(block: Block, pin: u32, func: &str) -> Result<bool, PinError> {
        let mut p = pinctrl(block)?;
        let _l = RMW.lock();
        p.set_function(pin, func)
    }
    pub fn pin_set_pull(block: Block, pin: u32, pull: Pull) -> Result<bool, PinError> {
        let mut p = pinctrl(block)?;
        let _l = RMW.lock();
        p.set_pull(pin, pull)
    }
    pub fn pin_get_fsel(block: Block, pin: u32) -> Result<u32, PinError> {
        pinctrl(block)?.get_fsel(pin).ok_or(PinError::NoPin)
    }
    pub fn pin_get_function(block: Block, pin: u32) -> Result<&'static str, PinError> {
        pinctrl(block)?.get_function(pin).ok_or(PinError::NoPin)
    }
    /// Apply a pin group (e.g. [`GROUP_UARTA`]); returns the register writes made.
    pub fn pin_apply(block: Block, group: &[PinConf]) -> Result<u32, PinError> {
        let mut p = pinctrl(block)?;
        let _l = RMW.lock();
        p.apply(group)
    }

    // ── chained interrupts ──

    const NL2: usize = L2_CONTROLLERS.len();
    struct Slot { h: AtomicUsize, ctx: AtomicUsize }
    const SLOT0: Slot = Slot { h: AtomicUsize::new(0), ctx: AtomicUsize::new(0) };
    static L2_CHILD: [[Slot; 32]; NL2] = [const { [SLOT0; 32] }; NL2];
    static L2_UP: [AtomicBool; NL2] = [const { AtomicBool::new(false) }; NL2];
    static L2_MASKED: [AtomicU32; NL2] = [const { AtomicU32::new(0) }; NL2];
    static GIO_CHILD: [Slot; 64] = [SLOT0; 64];
    static GIO_UP: AtomicBool = AtomicBool::new(false);
    static IRQ_LOCK: Mutex<()> = Mutex::new(());

    fn call(s: &Slot) -> bool {
        let h = s.h.load(Ordering::Acquire);
        if h == 0 { return false; }
        let f: IrqHandler = unsafe { core::mem::transmute(h) };
        f(s.ctx.load(Ordering::Relaxed));
        true
    }

    fn l2_regs(i: usize) -> Option<L2<Mmio>> {
        let b = BASE[4 + i].load(Ordering::Acquire);
        (b != 0).then(|| L2::new(Mmio(b), L2_CONTROLLERS[i].kind))
    }

    extern "C" fn l2_isr(i: usize) -> IrqReturn {
        let Some(mut l2) = l2_regs(i) else { return IrqReturn::None };
        let (n, masked) = l2.dispatch(|hw| call(&L2_CHILD[i][hw as usize]));
        if masked != 0 { L2_MASKED[i].fetch_or(masked, Ordering::Relaxed); }
        if n + masked.count_ones() > 0 { IrqReturn::Handled } else { IrqReturn::None }
    }

    /// Route l2 source `hwirq` of `id` to `handler(ctx)` (hard-IRQ context; the
    /// handler must quiesce its device). The first request on a controller
    /// masks all its sources and claims its GIC SPI.
    pub fn l2_request(id: L2Id, hwirq: u32, handler: IrqHandler, ctx: usize) -> Result<(), GpioError> {
        if DEAD.load(Ordering::Relaxed) { return Err(GpioError::Unavailable); }
        if hwirq >= 32 { return Err(GpioError::BadLine); }
        let i = id as usize;
        let d = &L2_CONTROLLERS[i];
        let _g = IRQ_LOCK.lock();
        let s = &L2_CHILD[i][hwirq as usize];
        if s.h.load(Ordering::Acquire) != 0 { return Err(GpioError::Busy); }
        if !L2_UP[i].load(Ordering::Acquire) {
            map_one(4 + i, d.phys, 0x18).ok_or(GpioError::Unavailable)?;
            l2_regs(i).ok_or(GpioError::Unavailable)?.reset();
            crate::irq::request_irq(IrqSource::Line(spi_intid(d.spi)), l2_isr, i).map_err(|_| GpioError::NoIrq)?;
            L2_UP[i].store(true, Ordering::Release);
        }
        s.ctx.store(ctx, Ordering::Relaxed);
        s.h.store(handler as usize, Ordering::Release);
        L2_MASKED[i].fetch_and(!(1 << hwirq), Ordering::Relaxed);
        l2_regs(i).ok_or(GpioError::Unavailable)?.unmask(hwirq);
        Ok(())
    }

    pub fn l2_free(id: L2Id, hwirq: u32) {
        let i = id as usize;
        if hwirq >= 32 { return; }
        let _g = IRQ_LOCK.lock();
        if let Some(mut l2) = l2_regs(i) { l2.mask(hwirq); }
        L2_CHILD[i][hwirq as usize].h.store(0, Ordering::Release);
    }

    extern "C" fn gio_isr(_ctx: usize) -> IrqReturn {
        let b = BASE[0].load(Ordering::Acquire);
        if b == 0 { return IrqReturn::None; }
        let mut g = Gpio::new(Mmio(b), bank_widths(Chip::Gio, stepping()));
        let n = g.dispatch(|line| { call(&GIO_CHILD[line as usize]); });
        if n > 0 { IrqReturn::Handled } else { IrqReturn::None }
    }

    /// Interrupt on a `gio` line (gio -> main_irq hwirq 0 -> GIC SPI 244).
    /// `gio_aon` has no interrupt wired in the D0 tree.
    pub fn gpio_request_irq(line: u32, trig: Trigger, handler: IrqHandler, ctx: usize) -> Result<(), GpioError> {
        let mut g = gpio(Chip::Gio)?;
        locate(g.widths, line)?;
        let _g = IRQ_LOCK.lock();
        let s = &GIO_CHILD[line as usize];
        if s.h.load(Ordering::Acquire) != 0 { return Err(GpioError::Busy); }
        if !GIO_UP.load(Ordering::Acquire) {
            { let _l = RMW.lock(); g.irq_reset(); }
            drop(_g);
            l2_request(L2Id::MainIrq, 0, gio_isr, 0)?;
            GIO_UP.store(true, Ordering::Release);
            return gpio_request_irq(line, trig, handler, ctx);
        }
        s.ctx.store(ctx, Ordering::Relaxed);
        s.h.store(handler as usize, Ordering::Release);
        let _l = RMW.lock();
        g.set_trigger(line, trig)?;
        g.ack(line)?;
        g.set_irq_enabled(line, true)
    }

    pub fn gpio_free_irq(line: u32) {
        if let Ok(mut g) = gpio(Chip::Gio) {
            let _g = IRQ_LOCK.lock();
            let _l = RMW.lock();
            let _ = g.set_irq_enabled(line, false);
            if let Some(s) = GIO_CHILD.get(line as usize) { s.h.store(0, Ordering::Release); }
        }
    }

    pub fn l2_masked_spurious(id: L2Id) -> u32 { L2_MASKED[id as usize].load(Ordering::Relaxed) }

    extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }
    fn log(s: &str) { unsafe { serial_print_str_raw(s.as_ptr(), s.len()) } }

    fn dir(chip: Chip, line: u32) -> alloc::string::String {
        match (gpio_is_output(chip, line), gpio_get(chip, line)) {
            (Ok(o), Ok(v)) => alloc::format!("{}:{}", if o { "out" } else { "in" }, v as u8),
            _ => alloc::string::String::from("?"),
        }
    }

    /// Boot probe: map the four blocks, read (never write) WL_ON / BT_ON and
    /// the BT uart mux, and print one `[gpio]` line. `checkpoint` is the
    /// arch SError checkpoint: an abort taken while reading marks the driver
    /// dead (every API call then returns Unavailable) instead of being pinned
    /// on a later probe. The l2 controllers stay untouched until requested.
    pub fn boot(checkpoint: fn() -> Option<u64>) {
        if let Some(syn) = checkpoint() {
            log(&alloc::format!("[gpio] serror pending before probe syn={:#x} (earlier probe)\n", syn));
        }
        let si = crate::bcm2712_id::stepping();
        let gw = line_count(bank_widths(Chip::Gio, si.stepping));
        let aw = line_count(bank_widths(Chip::GioAon, si.stepping));
        let (wl, bt) = (dir(Chip::Gio, WL_ON), dir(Chip::Gio, BT_ON));
        let uart: alloc::vec::Vec<&str> = (24..28).map(|p| pin_get_function(Block::Main, p).unwrap_or("?")).collect();
        let aon = pin_get_function(Block::Aon, 0).unwrap_or("?");
        if let Some(syn) = checkpoint() {
            DEAD.store(true, Ordering::Relaxed);
            log(&alloc::format!("[gpio] FAIL serror syn={:#x}; gpio/pinctrl disabled\n", syn));
            return;
        }
        log(&alloc::format!(
            "[gpio] stepping={:?}({:?}) gio={} gio_aon={} wl_on={} bt_on={} uarta24-27={},{},{},{} aon_gpio0={} l2=main_irq:spi{},bsc_irq:spi{},cpu_l2:spi{} ok\n",
            si.stepping, si.source, gw, aw, wl, bt, uart[0], uart[1], uart[2], uart[3], aon,
            L2_CONTROLLERS[0].spi, L2_CONTROLLERS[1].spi, L2_CONTROLLERS[2].spi));
    }
}
