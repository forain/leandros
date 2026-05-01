//! Limine boot protocol — response structures and parser.
//!
//! Ref: Limine Boot Protocol Specification
//!      https://github.com/limine-bootloader/limine/blob/stable/PROTOCOL.md

use super::{BootInfo, MemoryRegion, MemoryType};
use core::cell::UnsafeCell;

// ── Limine Request / Response structures ─────────────────────────────────────

#[repr(C)]
pub struct Request<T> {
    pub id:       [u64; 4],
    pub revision: u64,
    pub response: UnsafeCell<*const T>,
}

impl<T> Request<T> {
    pub fn response(&self) -> Option<&T> {
        let ptr = unsafe { *self.response.get() };
        if ptr.is_null() { None } else { Some(unsafe { &*ptr }) }
    }
}

unsafe impl<T> Sync for Request<T> {}

#[repr(C)]
pub struct HhdmResponse {
    pub revision: u64,
    pub offset:   u64,
}

#[repr(C)]
pub struct MemMapResponse {
    pub revision:    u64,
    pub entry_count: u64,
    pub entries:     *const *const MemMapEntry,
}

#[repr(C)]
pub struct MemMapEntry {
    pub base:   u64,
    pub length: u64,
    pub typ:    u64,
}

const USABLE:           u64 = 0;
const ACPI_RECLAIMABLE: u64 = 1;
const ACPI_NVS:         u64 = 2;
const BAD_MEMORY:       u64 = 3;

#[repr(C)]
pub struct FramebufferResponse {
    pub revision:          u64,
    pub framebuffer_count: u64,
    pub framebuffers:      *const *const Framebuffer,
}

#[repr(C)]
pub struct Framebuffer {
    pub address: u64,
    pub width:   u32,
    pub height:  u32,
    pub pitch:   u32,
    pub bpp:     u16,
    pub memory_model: u8,
    pub red_mask_size:   u8,
    pub red_mask_shift:  u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size:  u8,
    pub blue_mask_shift: u8,
    pub unused: [u8; 7],
    pub edid_size: u64,
    pub edid: *const u8,
}

#[repr(C)]
pub struct RsdpResponse {
    pub revision: u64,
    pub address:  u64,
}

#[repr(C)]
pub struct ModuleResponse {
    pub revision:     u64,
    pub module_count: u64,
    pub modules:      *const *const Module,
}

#[repr(C)]
pub struct Module {
    pub revision: u64,
    pub address:  u64,
    pub size:     u64,
    pub path:     *const u8,
    pub cmdline:  *const u8,
    pub media_type: u32,
    pub unused:     u32,
    pub tftp_ip:    u32,
    pub tftp_port:  u32,
    pub partition_index: u32,
    pub tftp_err_no:     u32,
    pub tftp_err_str:    u32,
}

#[repr(C)]
pub struct EntryPointRequest {
    pub id:       [u64; 4],
    pub revision: u64,
    pub response: UnsafeCell<*const EntryPointResponse>,
    pub entry_point: extern "C" fn(usize) -> !,
}

unsafe impl Sync for EntryPointRequest {}

#[repr(C)]
pub struct EntryPointResponse {
    pub revision: u64,
}

#[repr(C)]
pub struct KernelAddressResponse {
    pub revision:      u64,
    pub physical_base: u64,
    pub virtual_base:  u64,
}

// ── Static storage for the parsed memory map ──────────────────────────────────
static mut MM: [MemoryRegion; 128] = [MemoryRegion { base: 0, length: 0, kind: MemoryType::Reserved }; 128];

// ── External requests (defined in kernel crate's main.rs) ──────────────────────

extern "C" {
    pub static HHDM_REQUEST: Request<HhdmResponse>;
    pub static MEMMAP_REQUEST: Request<MemMapResponse>;
    pub static FRAMEBUFFER_REQUEST: Request<FramebufferResponse>;
    pub static RSDP_REQUEST: Request<RsdpResponse>;
    pub static MODULE_REQUEST: Request<ModuleResponse>;
}

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
        let n = (resp.entry_count as usize).min(512);
        for i in 0..n {
            if idx >= 128 { break; }
            let e_ptr = *resp.entries.add(i);
            if e_ptr.is_null() { continue; }
            let e = &*e_ptr;

            let kind = match e.typ {
                USABLE           => MemoryType::Available,
                ACPI_RECLAIMABLE => MemoryType::AcpiReclaimable,
                ACPI_NVS         => MemoryType::AcpiNvs,
                BAD_MEMORY       => MemoryType::BadMemory,
                _                => MemoryType::Reserved,
            };
            MM[idx] = MemoryRegion { base: e.base, length: e.length, kind };
            idx += 1;
        }
        info.memory_map     = core::ptr::addr_of!(MM) as *const MemoryRegion;
        info.memory_map_len = idx;
    }

    if let Some(resp) = FRAMEBUFFER_REQUEST.response() {
        if resp.framebuffer_count > 0 {
            let fb = &**resp.framebuffers;
            info.framebuffer_base   = fb.address as u64;
            info.framebuffer_width  = fb.width  as u32;
            info.framebuffer_height = fb.height as u32;
            info.framebuffer_pitch  = fb.pitch  as u32;
        }
    }

    if let Some(resp) = RSDP_REQUEST.response() {
        info.rsdp_addr = resp.address as u64;
    }

    if let Some(resp) = MODULE_REQUEST.response() {
        if resp.module_count > 0 {
            let module = &**resp.modules;
            info.initrd_base = (module.address as u64).saturating_sub(info.hhdm_offset);
            info.initrd_size = module.size;
        }
    }

    info
}
