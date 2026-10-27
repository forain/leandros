//! BCM2712 PM block (`pm: watchdog@7d200000`, `brcm,bcm2712-pm`): the one
//! shared accessor. The V3D power-up (GRAFX island, `v3d/power.rs`) and the
//! watchdog / reset / halt code (`rpi_misc.rs`, kernel `power.rs`) both use
//! this window; every write goes through [`with`] so read-modify-write
//! sequences on PM registers never interleave.

use core::sync::atomic::{AtomicUsize, Ordering};
use leandros_lib::mmio::{MmioRd, MmioWr};
use spin::Mutex;

use crate::rpi_misc_logic::{pm, Regs32};

/// Physical base (soc `ranges` maps 0x7c000000 → 0x10_7c000000).
pub const PM_PHYS: u64 = 0x10_7D20_0000;
pub const PM_SIZE: usize = 0x1000;

static VIRT: AtomicUsize = AtomicUsize::new(0);
static LOCK: Mutex<()> = Mutex::new(());

/// Kernel virtual base of the PM window, ioremapped on first use.
pub fn base() -> Option<usize> {
    let v = VIRT.load(Ordering::Acquire);
    if v != 0 { return Some(v); }
    let m = mm::kva::ioremap(PM_PHYS, PM_SIZE, mm::kva::CacheMode::Uncached)?;
    match VIRT.compare_exchange(0, m, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Some(m),
        Err(prev) => Some(prev), // lost a race; the extra mapping is harmless
    }
}

/// Register window over the mapped PM block.
pub struct PmRegs(usize);

impl Regs32 for PmRegs {
    fn r(&mut self, off: usize) -> u32 { unsafe { ((self.0 + off) as *const u32).mmio_read() } }
    fn w(&mut self, off: usize, v: u32) { unsafe { ((self.0 + off) as *mut u32).mmio_write(v) } }
}

/// Run `f` on the PM registers under the PM lock. None if the block is unmapped.
pub fn with<R>(f: impl FnOnce(&mut PmRegs) -> R) -> Option<R> {
    let b = base()?;
    let _g = LOCK.lock();
    Some(f(&mut PmRegs(b)))
}

/// Last-resort lock-free variant for the reset path (another CPU may have
/// died holding the lock).
fn with_nolock<R>(f: impl FnOnce(&mut PmRegs) -> R) -> Option<R> {
    let b = VIRT.load(Ordering::Acquire);
    if b == 0 { return None; }
    Some(f(&mut PmRegs(b)))
}

/// Arm the watchdog: a full reset unless kicked within `secs` (1..=15).
pub fn wdt_start(secs: u32) -> bool { with(|r| pm::arm(r, pm::secs_to_ticks(secs))).is_some() }
pub fn wdt_kick(secs: u32) { let _ = with(|r| pm::arm(r, pm::secs_to_ticks(secs))); }
pub fn wdt_stop() { let _ = with(|r| pm::disarm(r)); }

/// Reset through the watchdog (fallback when PSCI SYSTEM_RESET returns).
pub fn restart() -> bool { with_nolock(|r| pm::restart(r)).is_some() }
/// Firmware halt (partition 63) + reset (fallback when PSCI SYSTEM_OFF returns).
pub fn power_off() -> bool { with_nolock(|r| pm::power_off(r)).is_some() }
