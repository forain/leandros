//! PCI configuration space and port I/O for RM.
//!
//! The handle RM keeps (`nv->handle`) is a boxed `PciFunction`. Accesses use
//! `PciFunction::read*/write*`, which reach the full 4 KiB configuration space
//! (legacy 0xCF8 below 0x100, ACPI-MCFG ECAM above). The function's ECAM page
//! is mapped once in task context when the handle is created, so extended
//! reads from the ISR never see an unmapped page.
//!
//! Every config write is logged (first 128): RM rarely writes config space
//! and each one matters on the bring-up box.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use drivers::pci::PciFunction;

use crate::{nvlog, NV_ERR_INVALID_ARGUMENT, NV_ERR_NOT_SUPPORTED, NV_OK};

static WRITE_LOG_BUDGET: AtomicU32 = AtomicU32::new(128);

pub(crate) struct PciHandle {
    pub f: PciFunction,
}

/// C: `void *os_pci_init_handle(NvU32 domain, NvU8 bus, NvU8 slot, NvU8 fn,
/// NvU16 *vendor, NvU16 *device)`. NULL when nothing answers there.
#[no_mangle]
pub unsafe extern "C" fn os_pci_init_handle(domain: u32, bus: u8, slot: u8, func: u8, vendor: *mut u16, device: *mut u16) -> *mut c_void {
    cb!("os_pci_init_handle");
    if domain != 0 || slot >= 32 || func >= 8 {
        return core::ptr::null_mut();
    }
    let f = PciFunction::new(bus, slot, func);
    let vid = f.read16(0);
    if vid == 0xFFFF {
        return core::ptr::null_mut();
    }
    if !vendor.is_null() {
        *vendor = vid;
    }
    if !device.is_null() {
        *device = f.read16(2);
    }
    let ext = f.has_extended_config();
    nvlog!("[nvrm] +{}ms os_pci_init_handle {:02x}:{:02x}.{} {:04x}:{:04x} extended-config={}\n",
           crate::ms(), bus, slot, func, vid, f.read16(2), if ext { "ECAM" } else { "NO (offsets >= 0x100 read as ~0)" });
    alloc::boxed::Box::into_raw(alloc::boxed::Box::new(PciHandle { f })) as *mut c_void
}

/// Linux removes the device from the bus (GPU fell off). Keep the handle:
/// nv->handle still points at it.
#[no_mangle]
pub unsafe extern "C" fn os_pci_remove(_handle: *mut c_void) {
    cb!("os_pci_remove");
    nvlog!("[nvrm] +{}ms os_pci_remove requested (GPU lost?); ignored\n", crate::ms());
}

#[no_mangle]
pub unsafe extern "C" fn os_pci_remove_supported() -> u8 {
    cb!("os_pci_remove_supported");
    0
}

unsafe fn func<'a>(h: *mut c_void) -> Option<&'a PciFunction> {
    if h.is_null() { None } else { Some(&(*(h as *const PciHandle)).f) }
}

unsafe fn rd<T>(name: &str, h: *mut c_void, off: u32, out: *mut T, w: usize, get: impl Fn(&PciFunction, u16) -> T) -> u32 {
    let _ = name;
    let f = match func(h) { Some(f) => f, None => return NV_ERR_INVALID_ARGUMENT };
    if off as usize + w > 4096 || out.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    *out = get(f, off as u16);
    NV_OK
}

unsafe fn wr(name: &str, h: *mut c_void, off: u32, v: u32, w: usize, put: impl Fn(&PciFunction, u16)) -> u32 {
    let f = match func(h) { Some(f) => f, None => return NV_ERR_INVALID_ARGUMENT };
    if off as usize + w > 4096 {
        return NV_ERR_INVALID_ARGUMENT;
    }
    if WRITE_LOG_BUDGET.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1)).is_ok() {
        nvlog!("[nvrm] +{}ms {} {:02x}:{:02x}.{} [{:#05x}] = {:#x}\n",
               crate::ms(), name, f.bus, f.dev, f.func, off, v);
    }
    put(f, off as u16);
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_pci_read_byte(h: *mut c_void, off: u32, out: *mut u8) -> u32 {
    cb!("os_pci_read_byte");
    rd("os_pci_read_byte", h, off, out, 1, |f, o| f.read8(o))
}

#[no_mangle]
pub unsafe extern "C" fn os_pci_read_word(h: *mut c_void, off: u32, out: *mut u16) -> u32 {
    cb!("os_pci_read_word");
    rd("os_pci_read_word", h, off, out, 2, |f, o| f.read16(o))
}

#[no_mangle]
pub unsafe extern "C" fn os_pci_read_dword(h: *mut c_void, off: u32, out: *mut u32) -> u32 {
    cb!("os_pci_read_dword");
    rd("os_pci_read_dword", h, off, out, 4, |f, o| f.read32(o))
}

#[no_mangle]
pub unsafe extern "C" fn os_pci_write_byte(h: *mut c_void, off: u32, v: u8) -> u32 {
    cb!("os_pci_write_byte");
    wr("os_pci_write_byte", h, off, v as u32, 1, |f, o| f.write8(o, v))
}

#[no_mangle]
pub unsafe extern "C" fn os_pci_write_word(h: *mut c_void, off: u32, v: u16) -> u32 {
    cb!("os_pci_write_word");
    wr("os_pci_write_word", h, off, v as u32, 2, |f, o| f.write16(o, v))
}

#[no_mangle]
pub unsafe extern "C" fn os_pci_write_dword(h: *mut c_void, off: u32, v: u32) -> u32 {
    cb!("os_pci_write_dword");
    wr("os_pci_write_dword", h, off, v, 4, |f, o| f.write32(o, v))
}

#[no_mangle]
pub unsafe extern "C" fn os_enable_pci_req_atomics(_h: *mut c_void, _kind: u32) -> u32 {
    cb!("os_enable_pci_req_atomics");
    NV_ERR_NOT_SUPPORTED
}

// --------------------------------------------------------------- port I/O --
// RM touches legacy VGA ports on a primary-VGA adapter. Real port I/O, as
// Linux's inb/outb (nvidia-haiku panics here; it is never reached there).

#[cfg(target_arch = "x86_64")]
mod pio {
    use core::arch::asm;
    pub unsafe fn inb(p: u16) -> u8 { let v: u8; asm!("in al, dx", out("al") v, in("dx") p, options(nomem, nostack)); v }
    pub unsafe fn inw(p: u16) -> u16 { let v: u16; asm!("in ax, dx", out("ax") v, in("dx") p, options(nomem, nostack)); v }
    pub unsafe fn inl(p: u16) -> u32 { let v: u32; asm!("in eax, dx", out("eax") v, in("dx") p, options(nomem, nostack)); v }
    pub unsafe fn outb(p: u16, v: u8) { asm!("out dx, al", in("dx") p, in("al") v, options(nomem, nostack)); }
    pub unsafe fn outw(p: u16, v: u16) { asm!("out dx, ax", in("dx") p, in("ax") v, options(nomem, nostack)); }
    pub unsafe fn outl(p: u16, v: u32) { asm!("out dx, eax", in("dx") p, in("eax") v, options(nomem, nostack)); }
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn os_io_read_byte(port: u32) -> u8 {
    cb!("os_io_read_byte");
    pio::inb(port as u16)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn os_io_read_word(port: u32) -> u16 {
    cb!("os_io_read_word");
    pio::inw(port as u16)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn os_io_read_dword(port: u32) -> u32 {
    cb!("os_io_read_dword");
    pio::inl(port as u16)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn os_io_write_byte(port: u32, v: u8) {
    cb!("os_io_write_byte");
    pio::outb(port as u16, v)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn os_io_write_word(port: u32, v: u16) {
    cb!("os_io_write_word");
    pio::outw(port as u16, v)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn os_io_write_dword(port: u32, v: u32) {
    cb!("os_io_write_dword");
    pio::outl(port as u16, v)
}
