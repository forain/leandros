//! Limine boot protocol — response structures and parser.
//!
//! Ref: Limine Boot Protocol Specification
//!      https://github.com/limine-bootloader/limine/blob/stable/PROTOCOL.md

use super::{BootInfo, MemoryRegion, MemoryType};
use core::cell::UnsafeCell;

// ── Request structures ────────────────────────────────────────────────────────

#[repr(C, align(8))]
pub struct Request<T> {
    pub id:       [u64; 4],
    pub revision: u64,
    pub response: UnsafeCell<*const T>,
}

#[repr(C, align(8))]
pub struct EntryPointRequest {
    pub id:          [u64; 4],
    pub revision:    u64,
    pub response:    UnsafeCell<*const u8>,
    pub entry_point: unsafe extern "C" fn() -> !,
}

// SAFETY: response is written once by the bootloader.
unsafe impl<T> Sync for Request<T> {}
unsafe impl Sync for EntryPointRequest {}

impl<T> Request<T> {
    /// Return the response pointer if Limine satisfied this request.
    pub unsafe fn response(&self) -> Option<&T> {
        let resp = *self.response.get();
        if resp.is_null() {
            None
        } else {
            Some(&*resp)
        }
    }
}

// ── Response layouts ──────────────────────────────────────────────────────────

#[repr(C, align(8))]
pub struct HhdmResponse {
    pub revision: u64,
    pub offset:   u64,
}

#[repr(C, align(8))]
pub struct MemMapResponse {
    pub revision:    u64,
    pub entry_count: u64,
    pub entries:     *const *const MemMapEntry,
}

#[repr(C, align(8))]
pub struct MemMapEntry {
    pub base:   u64,
    pub length: u64,
    pub typ:    u64,
}

const USABLE:                u64 = 0;
const ACPI_RECLAIMABLE:      u64 = 2;
const ACPI_NVS:              u64 = 3;
const BAD_MEMORY:            u64 = 4;

#[repr(C, align(8))]
pub struct FramebufferResponse {
    pub revision:        u64,
    pub framebuffer_count: u64,
    pub framebuffers:    *const *const LimineFramebuffer,
}

#[repr(C, align(8))]
pub struct LimineFramebuffer {
    pub address:         *mut u8,
    pub width:           u64,
    pub height:          u64,
    pub pitch:           u64,
    pub bpp:             u16,
    pub memory_model:    u8,
    pub red_mask_size:   u8,
    pub red_mask_shift:  u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size:  u8,
    pub blue_mask_shift: u8,
    pub _unused:         [u8; 7],
    pub edid_size:       u64,
    pub edid:            *const u8,
}

#[repr(C, align(8))]
pub struct RsdpResponse {
    pub revision: u64,
    pub address:  *const u8,
}

#[repr(C, align(8))]
pub struct ModuleResponse {
    pub revision:     u64,
    pub module_count: u64,
    pub modules:      *const *const Module,
}

#[repr(C, align(8))]
pub struct Module {
    pub revision: u64,
    pub address:  *const u8,
    pub size:     u64,
    pub path:     *const u8,
    pub cmdline:  *const u8,
    pub media_type: u32,
    pub unused:   u32,
    pub tftp_ip: u32,
    pub tftp_port: u32,
    pub partition_index: u32,
    pub mbr_disk_id: u32,
    pub gpt_disk_uuid: [u8; 16],
    pub gpt_part_uuid: [u8; 16],
    pub part_uuid: [u8; 16],
}

// ── Static memory-map storage ─────────────────────────────────────────────────

static mut MM: [MemoryRegion; 128] = [MemoryRegion {
    base: 0, length: 0, kind: MemoryType::Reserved,
}; 128];

// ── Public API ────────────────────────────────────────────────────────────────

extern "C" {
    static HHDM_REQUEST: Request<HhdmResponse>;
    static MEMMAP_REQUEST: Request<MemMapResponse>;
    static FRAMEBUFFER_REQUEST: Request<FramebufferResponse>;
    static RSDP_REQUEST: Request<RsdpResponse>;
    static MODULE_REQUEST: Request<ModuleResponse>;
}

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
            let e = &**resp.entries.add(i);
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
            info.initrd_base = module.address as u64;
            info.initrd_size = module.size;
        }
    }

    info
}
