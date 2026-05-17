//! Limine boot protocol handler using the official `limine` crate.

use super::{BootInfo, MemoryRegion, MemoryType};
use limine::request::{
    HhdmRequest, MemmapRequest, FramebufferRequest, RsdpRequest, ModulesRequest, ExecutableAddressRequest, DtbRequest
};

static mut LIMINE_REGIONS: [MemoryRegion; 256] = [MemoryRegion { 
    base: 0, 
    length: 0, 
    kind: MemoryType::Reserved 
}; 256];

/// Parse Limine boot information using explicit request references.
///
/// # Safety
/// The requests must have been populated by a Limine-compliant bootloader.
pub unsafe fn parse_with_requests(
    hhdm:           &HhdmRequest,
    memmap:         &MemmapRequest,
    framebuffer:    &FramebufferRequest,
    modules:        &ModulesRequest,
    rsdp:           &RsdpRequest,
    _kernel_addr:   &ExecutableAddressRequest,
    dtb:            &DtbRequest,
) -> BootInfo {
    let mut info = BootInfo {
        memory_map:          core::ptr::null(),
        memory_map_len:      0,
        framebuffer_base:    0,
        framebuffer_width:   0,
        framebuffer_height:  0,
        framebuffer_pitch:   0,
        rsdp_addr:           0,
        uart_base:           0,
        initrd_base:         0,
        initrd_size:         0,
        hhdm_offset:         0,
    };

    if let Some(resp) = hhdm.response() {
        info.hhdm_offset = resp.offset;
    }

    if let Some(resp) = memmap.response() {
        let entries = resp.entries();
        let count = core::cmp::min(entries.len(), 256);
        for i in 0..count {
            let entry = entries[i];
            let kind = match entry.type_ as u64 {
                0 => MemoryType::Available,       // USABLE
                2 => MemoryType::AcpiReclaimable, // ACPI_RECLAIMABLE
                3 => MemoryType::AcpiNvs,         // ACPI_NVS
                4 => MemoryType::BadMemory,       // BAD_MEMORY
                _ => MemoryType::Reserved,
            };
            LIMINE_REGIONS[i] = MemoryRegion {
                base: entry.base,
                length: entry.length,
                kind,
            };
        }
        info.memory_map = core::ptr::addr_of!(LIMINE_REGIONS) as *const MemoryRegion;
        info.memory_map_len = count;
    }

    if let Some(resp) = framebuffer.response() {
        if let Some(fb) = resp.framebuffers().first() {
            // Limine provides a virtual address in the HHDM.
            // We store the physical address in BootInfo for consistency.
            info.framebuffer_base   = fb.address() as u64 - info.hhdm_offset;
            info.framebuffer_width  = fb.width as u32;
            info.framebuffer_height = fb.height as u32;
            info.framebuffer_pitch  = fb.pitch as u32;
        }
    }

    if let Some(resp) = rsdp.response() {
        info.rsdp_addr = resp.address as u64;
    }

    if let Some(resp) = modules.response() {
        for module in resp.modules() {
            if module.cmdline().starts_with("initrd") {
                // Limine provides a virtual address in the HHDM.
                // We store the physical address in BootInfo for consistency.
                info.initrd_base = module.data().as_ptr() as u64 - info.hhdm_offset;
                info.initrd_size = module.data().len() as u64;
            }
        }
    }

    if let Some(resp) = dtb.response() {
        if !resp.dtb_ptr.is_null() {
            info.uart_base = 0x09000000; // Default for QEMU virt
        }
    }

    info
}

/// Legacy parse function.
pub unsafe fn parse() -> BootInfo {
    BootInfo {
        memory_map:          core::ptr::null(),
        memory_map_len:      0,
        framebuffer_base:    0,
        framebuffer_width:   0,
        framebuffer_height:  0,
        framebuffer_pitch:   0,
        rsdp_addr:           0,
        uart_base:           0,
        initrd_base:         0,
        initrd_size:         0,
        hhdm_offset:         0,
    }
}
