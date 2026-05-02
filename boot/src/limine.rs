//! Limine boot protocol handler using the official `limine` crate.

use super::{BootInfo, MemoryRegion, MemoryType};
use limine::request::{
    HhdmRequest, MemmapRequest, FramebufferRequest, RsdpRequest, ModulesRequest, ExecutableAddressRequest
};

// ── Limine Requests ──────────────────────────────────────────────────────────

// Placing requests in the `.limine_reqs` section.
// Markers are handled in kernel/src/main.rs to ensure they are linked.

#[used]
#[link_section = ".limine_reqs"]
pub static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[link_section = ".limine_reqs"]
pub static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

#[used]
#[link_section = ".limine_reqs"]
pub static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[used]
#[link_section = ".limine_reqs"]
pub static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

#[used]
#[link_section = ".limine_reqs"]
pub static MODULE_REQUEST: ModulesRequest = ModulesRequest::new();

#[used]
#[link_section = ".limine_reqs"]
pub static KERNEL_ADDR_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

// ── Static storage for the parsed memory map ──────────────────────────────────
static mut MM: [MemoryRegion; 128] = [MemoryRegion { base: 0, length: 0, kind: MemoryType::Reserved }; 128];

// ── Parser ───────────────────────────────────────────────────────────────────

pub unsafe fn parse() -> BootInfo {
    let mut info = BootInfo {
        memory_map:         core::ptr::null(),
        memory_map_len:     0,
        framebuffer_base:   0,
        framebuffer_width:  0,
        framebuffer_height: 0,
        framebuffer_pitch:  0,
        rsdp_addr:          0,
        uart_base:          0,
        initrd_base:        0,
        initrd_size:        0,
        hhdm_offset:        0,
    };

    if let Some(resp) = HHDM_REQUEST.response() {
        info.hhdm_offset = resp.offset;
    }

    if let Some(resp) = MEMMAP_REQUEST.response() {
        let mut idx = 0usize;
        for e in resp.entries() {
            if idx >= 128 { break; }

            let kind = match e.type_ {
                0 => MemoryType::Available,       // Usable
                1 => MemoryType::Reserved,        // Reserved
                2 => MemoryType::AcpiReclaimable, // ACPI reclaimable
                3 => MemoryType::AcpiNvs,         // ACPI NVS
                4 => MemoryType::BadMemory,       // Bad memory
                5 => MemoryType::Available,       // Bootloader reclaimable
                6 => MemoryType::Reserved,        // Kernel and modules
                7 => MemoryType::Reserved,        // Framebuffer
                _ => MemoryType::Reserved,
            };
            MM[idx] = MemoryRegion { base: e.base, length: e.length, kind };
            idx += 1;
        }
        info.memory_map     = core::ptr::addr_of!(MM) as *const MemoryRegion;
        info.memory_map_len = idx;
    }

    if let Some(resp) = FRAMEBUFFER_REQUEST.response() {
        let framebuffers = resp.framebuffers();
        if !framebuffers.is_empty() {
            let fb = framebuffers[0];
            // Normalize address to physical.
            info.framebuffer_base = (fb.address() as u64).saturating_sub(info.hhdm_offset);
            info.framebuffer_width  = fb.width as u32;
            info.framebuffer_height = fb.height as u32;
            info.framebuffer_pitch  = fb.pitch as u32;
        }
    }

    if let Some(resp) = RSDP_REQUEST.response() {
        info.rsdp_addr = (resp.address as u64).saturating_sub(info.hhdm_offset);
    }

    if let Some(resp) = MODULE_REQUEST.response() {
        let modules = resp.modules();
        if !modules.is_empty() {
            let module = modules[0];
            info.initrd_base = (module.data().as_ptr() as u64).saturating_sub(info.hhdm_offset);
            info.initrd_size = module.data().len() as u64;
        }
    }

    info
}
