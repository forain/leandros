//! Device-MMIO accessors that never compile to writeback addressing.
//!
//! `read_volatile`/`write_volatile` only guarantee that ONE access happens,
//! not its addressing mode: on aarch64 LLVM may emit `ldrb w8, [x19, #2]!`
//! (base-register writeback). Such an access carries no instruction syndrome
//! (ESR ISV=0) when it traps on MMIO; HVF cannot emulate it and QEMU aborts
//! (`hvf_handle_exception: Assertion isv failed`). TCG and KVM do not care,
//! which is why it only shows on the Mac. These helpers emit a single
//! `ldr*`/`str*` with a plain `[reg]` address on aarch64 and fall back to
//! volatile accesses elsewhere. Use them for DEVICE memory only; volatile
//! reads of ordinary RAM (rings, page tables) are fine as they are.

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

/// Integer widths an MMIO register access can have.
pub trait Mmio: sealed::Sealed + Copy {
    unsafe fn mmio_read(p: *const Self) -> Self;
    unsafe fn mmio_write(p: *mut Self, v: Self);
}

macro_rules! imp {
    ($t:ty, $ld:literal, $st:literal, $rm:literal, $conv:ty) => {
        impl Mmio for $t {
            #[inline(always)]
            unsafe fn mmio_read(p: *const Self) -> Self {
                #[cfg(target_arch = "aarch64")]
                {
                    let v: $conv;
                    core::arch::asm!(concat!($ld, " {v", $rm, "}, [{p}]"),
                        v = out(reg) v, p = in(reg) p,
                        options(nostack, readonly, preserves_flags));
                    v as $t
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    core::ptr::read_volatile(p)
                }
            }
            #[inline(always)]
            unsafe fn mmio_write(p: *mut Self, v: Self) {
                #[cfg(target_arch = "aarch64")]
                {
                    core::arch::asm!(concat!($st, " {v", $rm, "}, [{p}]"),
                        v = in(reg) v as $conv, p = in(reg) p,
                        options(nostack, preserves_flags));
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    core::ptr::write_volatile(p, v)
                }
            }
        }
    };
}
imp!(u8, "ldrb", "strb", ":w", u32);
imp!(u16, "ldrh", "strh", ":w", u32);
imp!(u32, "ldr", "str", ":w", u32);
imp!(u64, "ldr", "str", "", u64);

/// Read one MMIO register of width `T`.
#[inline(always)]
pub unsafe fn read<T: Mmio>(p: *const T) -> T {
    T::mmio_read(p)
}
/// Write one MMIO register of width `T`.
#[inline(always)]
pub unsafe fn write<T: Mmio>(p: *mut T, v: T) {
    T::mmio_write(p, v)
}
#[inline(always)]
pub unsafe fn read8(p: *const u8) -> u8 { read(p) }
#[inline(always)]
pub unsafe fn read16(p: *const u16) -> u16 { read(p) }
#[inline(always)]
pub unsafe fn read32(p: *const u32) -> u32 { read(p) }
#[inline(always)]
pub unsafe fn read64(p: *const u64) -> u64 { read(p) }
#[inline(always)]
pub unsafe fn write8(p: *mut u8, v: u8) { write(p, v) }
#[inline(always)]
pub unsafe fn write16(p: *mut u16, v: u16) { write(p, v) }
#[inline(always)]
pub unsafe fn write32(p: *mut u32, v: u32) { write(p, v) }
#[inline(always)]
pub unsafe fn write64(p: *mut u64, v: u64) { write(p, v) }

/// Method-call spelling of `read`, so `p.read_volatile()` converts by renaming.
pub trait MmioRd<T: Mmio> {
    unsafe fn mmio_read(self) -> T;
}
impl<T: Mmio> MmioRd<T> for *const T {
    #[inline(always)]
    unsafe fn mmio_read(self) -> T { read(self) }
}
impl<T: Mmio> MmioRd<T> for *mut T {
    #[inline(always)]
    unsafe fn mmio_read(self) -> T { read(self as *const T) }
}
/// Method-call spelling of `write`.
pub trait MmioWr<T: Mmio> {
    unsafe fn mmio_write(self, v: T);
}
impl<T: Mmio> MmioWr<T> for *mut T {
    #[inline(always)]
    unsafe fn mmio_write(self, v: T) { write(self, v) }
}
