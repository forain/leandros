//! Limine boot protocol handler using the official `limine` crate.

use super::{BootInfo, MemoryRegion};
use limine::request::{
    HhdmRequest, MemmapRequest, FramebufferRequest, RsdpRequest, ModulesRequest, ExecutableAddressRequest, DtbRequest
};

/// Raw copy of Limine's map (kinds collapsed). `MAX_RAW` (1024) entries, not
/// the 256 it used to be: an entry past the cap was dropped silently. The
/// kernel sanitizes this (`sanitize_boot_info`) before mm sees it.
static mut LIMINE_REGIONS: [MemoryRegion; super::memmap::MAX_RAW] =
    [MemoryRegion::EMPTY; super::memmap::MAX_RAW];
static mut LIMINE_REPORTED: usize = 0;

/// One Limine memmap entry with its raw `type_` (0 usable, 1 reserved,
/// 2 ACPI reclaimable, 3 ACPI NVS, 4 bad, 5 bootloader reclaimable,
/// 6 executable+modules, 7 framebuffer). `LIMINE_REGIONS` folds 5/6/7 into
/// Reserved; kexec needs them to tell the next kernel they are RAM.
#[derive(Clone, Copy)]
pub struct RawEntry { pub base: u64, pub length: u64, pub raw_type: u32 }
static mut LIMINE_RAW: [RawEntry; super::memmap::MAX_RAW] =
    [RawEntry { base: 0, length: 0, raw_type: 0 }; super::memmap::MAX_RAW];
static mut LIMINE_RAW_LEN: usize = 0;

/// Limine's memory map with the raw entry types, as reported (empty when not
/// booted by Limine).
pub fn raw_regions() -> &'static [RawEntry] {
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(LIMINE_RAW) as *const RawEntry, LIMINE_RAW_LEN) }
}

/// How many entries Limine's memmap response reported (may exceed what was kept).
pub fn memmap_reported() -> usize { unsafe { LIMINE_REPORTED } }

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
        framebuffer_size:    0,
        rsdp_addr:           0,
        uart_base:           0,
        pci_ecam_base:       0,
        initrd_base:         0,
        initrd_size:         0,
        hhdm_offset:         0,
        cmdline_ptr:         0,
        cmdline_len:         0,
        efi_system_table:    0,
    };

    if let Some(resp) = hhdm.response() {
        info.hhdm_offset = resp.offset;
    }

    if let Some(resp) = memmap.response() {
        let entries = resp.entries();
        LIMINE_REPORTED = entries.len();
        let count = core::cmp::min(entries.len(), super::memmap::MAX_RAW);
        for i in 0..count {
            let entry = entries[i];
            LIMINE_RAW[i] = RawEntry { base: entry.base, length: entry.length, raw_type: entry.type_ as u32 };
            LIMINE_REGIONS[i] = MemoryRegion {
                base: entry.base,
                length: entry.length,
                kind: super::memmap::limine_kind(entry.type_ as u64),
            };
        }
        LIMINE_RAW_LEN = count;
        info.memory_map = core::ptr::addr_of!(LIMINE_REGIONS) as *const MemoryRegion;
        info.memory_map_len = count;
    }

    if let Some(resp) = framebuffer.response() {
        if let Some(fb) = resp.framebuffers().first() {
            let fb_virt = fb.address() as usize;
            #[cfg(target_arch = "aarch64")]
            let fb_phys = unsafe {
                let par: u64;
                core::arch::asm!(
                    "at s1e1r, {va}",
                    "isb",
                    "mrs {par}, par_el1",
                    va = in(reg) fb_virt,
                    par = out(reg) par,
                    options(nostack)
                );
                if par & 1 == 0 {
                    ((par & 0x0000_FFFF_FFFF_F000) as u64) | ((fb_virt & 0xFFF) as u64)
                } else {
                    fb_virt as u64 - info.hhdm_offset
                }
            };
            #[cfg(not(target_arch = "aarch64"))]
            let fb_phys = fb_virt as u64 - info.hhdm_offset;

            info.framebuffer_base   = fb_phys;
            info.framebuffer_width  = fb.width as u32;
            info.framebuffer_height = fb.height as u32;
            info.framebuffer_pitch  = fb.pitch as u32;
        }
    }

    if let Some(resp) = rsdp.response() {
        // Limine base revision 6 provides a virtual (HHDM) address; subtract the
        // offset to normalise to a physical address like all other BootInfo fields.
        let raw = resp.address as u64;
        info.rsdp_addr = if info.hhdm_offset != 0 && raw >= info.hhdm_offset {
            raw - info.hhdm_offset
        } else {
            raw
        };
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
            // Use the DTB parser to find addresses
            let dtb_info = super::device_tree::parse(resp.dtb_ptr as usize);
            info.uart_base = dtb_info.uart_base;
            info.pci_ecam_base = dtb_info.pci_ecam_base;
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
        framebuffer_size:    0,
        rsdp_addr:           0,
        uart_base:           0,
        pci_ecam_base:       0,
        initrd_base:         0,
        initrd_size:         0,
        hhdm_offset:         0,
        cmdline_ptr:         0,
        cmdline_len:         0,
        efi_system_table:    0,
    }
}
