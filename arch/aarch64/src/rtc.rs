//! PL031 real-time clock — the battery clock QEMU's `virt` machine exposes at
//! 0x0901_0000 (seeded from the host's UTC time). `RTCDR` at offset 0 is the
//! current time as whole seconds since the Unix epoch. Read once at boot to
//! give CLOCK_REALTIME an epoch; the counter (`timer::monotonic_ns`) carries
//! the clock from there.
//!
//! Only the virt build maps it: the Raspberry Pi boards have no PL031 and no
//! battery clock at all (their wall clock comes from the network), so there
//! `epoch_secs` answers `None` and CLOCK_REALTIME starts at the boot instant.
//!
//! Ref: ARM PrimeCell RTC (PL031) TRM, §3.2.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Physical base on QEMU virt.
pub const BASE: usize = 0x0901_0000;

/// Virtual address the register block is mapped at (0 = not mapped).
static VIRT_BASE: AtomicUsize = AtomicUsize::new(0);

/// Record the mapping made by `init` (device memory, one 4 KiB page).
pub fn set_base(virt: usize) {
    VIRT_BASE.store(virt, Ordering::Release);
}

/// Seconds since the Unix epoch from `RTCDR`, or `None` if the clock is not
/// mapped or reads as zero (no device behind the page).
pub fn epoch_secs() -> Option<u64> {
    let base = VIRT_BASE.load(Ordering::Acquire);
    if base == 0 { return None; }
    let v = unsafe { core::ptr::read_volatile(base as *const u32) };
    if v == 0 { None } else { Some(v as u64) }
}
