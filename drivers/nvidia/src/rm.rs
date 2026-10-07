//! Typed RM entry points (src/nvidia/arch/nvalloc/unix/include/nv.h) and the
//! per-GPU probe, following kernel-open/nvidia/nv-pci.c:nv_pci_probe minus
//! the Linux plumbing. The first argument of every rm_* call is Linux's
//! `nvidia_stack_t *` alt-stack cookie; it is unused on x86_64 and NULL here,
//! as in nvidia-haiku.

use core::ffi::{c_char, c_void};

use drivers::pci::{self, PciDevice};

use crate::glue::{self, NvState, NV_GPU_BAR_INDEX_FB, NV_GPU_BAR_INDEX_IMEM, NV_GPU_BAR_INDEX_REGS};
use crate::{nvlog, NV_OK};

pub type NvidiaStack = c_void;

extern "C" {
    pub fn rm_init_rm(sp: *mut NvidiaStack) -> bool;
    pub fn rm_shutdown_rm(sp: *mut NvidiaStack);
    pub fn rm_is_supported_device(sp: *mut NvidiaStack, nv: *mut NvState) -> u32;
    pub fn rm_init_private_state(sp: *mut NvidiaStack, nv: *mut NvState) -> bool;
    pub fn rm_free_private_state(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_set_rm_firmware_requested(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_init_event_locks(sp: *mut NvidiaStack, nv: *mut NvState) -> bool;
    pub fn rm_init_adapter(sp: *mut NvidiaStack, nv: *mut NvState) -> bool;
    pub fn rm_disable_adapter(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_shutdown_adapter(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_isr(sp: *mut NvidiaStack, nv: *mut NvState, need_bottom_half: *mut u32) -> bool;
    pub fn rm_isr_bh(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_get_device_name(device: u16, subsystem_vendor: u16, subsystem_device: u16) -> *const c_char;
}

/// OpenRM uses the PCI config offset of the BAR register (NVRM_PCICFG_BAR_OFFSET).
const PCI_BAR0: u8 = 0x10;
const PCI_COMMAND: u8 = 0x04;
const PCI_SUBSYSTEM_VENDOR: u8 = 0x2C;
const PCI_SUBSYSTEM_ID: u8 = 0x2E;
const PCI_INTERRUPT_LINE: u8 = 0x3C;

/// Decode and size a memory BAR. Returns (phys, size, next BAR index).
/// Sizing writes all-ones with memory decode disabled, then restores both.
unsafe fn size_bar(d: &PciDevice, idx: usize) -> (u64, u64, usize) {
    let off = PCI_BAR0 + (idx as u8) * 4;
    let lo = pci::pci_read_config_32(d.bus, d.dev, d.func, off);
    if lo & 1 != 0 {
        return (0, 0, idx + 1); // I/O BAR: RM ignores it
    }
    let is64 = (lo >> 1) & 3 == 2;
    let cmd = pci::pci_read_config_16(d.bus, d.dev, d.func, PCI_COMMAND);
    pci::pci_write_config_16(d.bus, d.dev, d.func, PCI_COMMAND, cmd & !0x3);

    pci::pci_write_config_32(d.bus, d.dev, d.func, off, 0xFFFF_FFFF);
    let mask_lo = pci::pci_read_config_32(d.bus, d.dev, d.func, off) & !0xF;
    pci::pci_write_config_32(d.bus, d.dev, d.func, off, lo);
    let (hi, mask_hi) = if is64 {
        let hi = pci::pci_read_config_32(d.bus, d.dev, d.func, off + 4);
        pci::pci_write_config_32(d.bus, d.dev, d.func, off + 4, 0xFFFF_FFFF);
        let m = pci::pci_read_config_32(d.bus, d.dev, d.func, off + 4);
        pci::pci_write_config_32(d.bus, d.dev, d.func, off + 4, hi);
        (hi, m)
    } else {
        (0, 0xFFFF_FFFF)
    };
    pci::pci_write_config_16(d.bus, d.dev, d.func, PCI_COMMAND, cmd);

    let phys = ((hi as u64) << 32) | (lo & !0xF) as u64;
    let mask = ((mask_hi as u64) << 32) | mask_lo as u64;
    let size = if mask_lo == 0 && mask_hi == 0 { 0 } else { (!mask).wrapping_add(1) };
    (phys, size, idx + if is64 { 2 } else { 1 })
}

/// nv_pci_probe equivalent: build the nv_state_t, hand it to RM, and request
/// GSP firmware. rm_init_adapter (GSP boot) happens later, on first open /
/// explicit start, exactly like Linux's nv_start_device.
pub unsafe fn probe_device(d: &PciDevice) -> Result<*mut NvState, &'static str> {
    let size = glue::lnv_state_size() as usize;
    let layout = core::alloc::Layout::from_size_align(size, 16).map_err(|_| "layout")?;
    let nv = alloc::alloc::alloc_zeroed(layout) as *mut NvState;
    if nv.is_null() {
        return Err("nv_state_t allocation failed");
    }

    let handle = crate::stubs::os_pci_init_handle(
        0, d.bus, d.dev, d.func, core::ptr::null_mut(), core::ptr::null_mut(),
    );
    glue::lnv_state_init(
        nv,
        core::ptr::null_mut(),
        handle,
        0,
        d.bus,
        d.dev,
        d.func,
        d.vendor_id,
        d.device_id,
        pci::pci_read_config_16(d.bus, d.dev, d.func, PCI_SUBSYSTEM_VENDOR),
        pci::pci_read_config_16(d.bus, d.dev, d.func, PCI_SUBSYSTEM_ID),
        pci::pci_read_config_8(d.bus, d.dev, d.func, PCI_INTERRUPT_LINE) as u32,
    );

    // GA104: BAR0 regs (32-bit, 16 MiB), BAR1 FB aperture (64-bit), BAR3
    // instance-memory aperture (64-bit). Assign in order, like nv-pci.c.
    let slots = [NV_GPU_BAR_INDEX_REGS, NV_GPU_BAR_INDEX_FB, NV_GPU_BAR_INDEX_IMEM];
    let mut next_slot = 0;
    let mut idx = 0;
    while idx < 6 && next_slot < slots.len() {
        let (phys, len, next) = size_bar(d, idx);
        if len != 0 {
            nvlog!("[nvrm]   BAR{} phys {:#x} size {:#x}\n", idx, phys, len);
            glue::lnv_state_set_bar(nv, slots[next_slot], idx as u32, phys, len);
            next_slot += 1;
        }
        idx = next;
    }

    // nv-pci.c order: nv_lock_init_locks (-> rm_init_event_locks) first.
    if !rm_init_event_locks(core::ptr::null_mut(), nv) {
        return Err("rm_init_event_locks failed");
    }
    if rm_is_supported_device(core::ptr::null_mut(), nv) != NV_OK {
        return Err("rm_is_supported_device rejected the GPU");
    }
    if !rm_init_private_state(core::ptr::null_mut(), nv) {
        return Err("rm_init_private_state failed");
    }
    rm_set_rm_firmware_requested(core::ptr::null_mut(), nv);
    nvlog!("[nvrm] private state ready; rm_init_adapter (GSP boot) is lane G2\n");
    Ok(nv)
}
