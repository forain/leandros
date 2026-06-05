//! Multiboot2 boot information parser — from multiboot2 spec §3.6
//!
//! GRUB (or QEMU -kernel) passes a pointer to the MBI in EBX (32-bit) /
//! RSI (64-bit after our trampoline).  We parse it here into our BootInfo.

use super::{BootInfo, MemoryRegion, MemoryType};

/// Multiboot2 magic passed in EAX by the bootloader.
pub const MULTIBOOT2_BOOTLOADER_MAGIC: u32 = 0x36d76289;

/// Tag type codes (§3.1.8).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagType {
    End           = 0,
    BootCmdLine   = 1,
    BootLoaderName = 2,
    Module        = 3,
    BasicMemInfo  = 4,
    BiosBoot      = 5,
    MemoryMap     = 6,
    VbeInfo       = 7,
    Framebuffer   = 8,
    ElfSections   = 9,
    ApmTable      = 10,
    Efi32         = 11,
    Efi64         = 12,
    SmbiosTables  = 13,
    AcpiV1        = 14,
    AcpiV2        = 15,
    Network       = 16,
    EfiMemMap     = 17,
    EfiBs         = 18,
    Efi32Ih       = 19,
    Efi64Ih       = 20,
    LoadBaseAddr  = 21,
}

/// Generic tag header (§3.1.7).
#[repr(C)]
struct TagHeader {
    typ:  u32,
    size: u32,
}

/// Memory map entry (§3.6.8).
#[repr(C)]
struct MbiMemMapEntry {
    base_addr: u64,
    length:    u64,
    typ:       u32,
    reserved:  u32,
}

/// Framebuffer info tag (§3.6.11).
#[repr(C)]
pub struct MbiFramebuffer {
    pub addr:   u64,
    pub pitch:  u32,
    pub width:  u32,
    pub height: u32,
    pub bpp:    u8,
    pub fb_type: u8,
}

/// Parse a multiboot2 info structure at `mbi_phys` and fill `out`.
///
/// # Safety
/// `mbi_phys` must be a valid physical address of a multiboot2 info block
/// as set up by a spec-compliant bootloader.
pub unsafe fn parse(mbi_phys: usize) -> BootInfo {
    let mut info = BootInfo {
        memory_map:          core::ptr::null(),
        memory_map_len:      0,
        framebuffer_base:    0,
        framebuffer_width:   0,
        framebuffer_height:  0,
        framebuffer_pitch:   0,
        rsdp_addr:           0,
        uart_base:           0,
        pci_ecam_base:       0,
        initrd_base:         0,
        initrd_size:         0,
        hhdm_offset:         0,
    };

    // MBI starts with total_size (u32) + reserved (u32), then tags.
    let total_size = (mbi_phys as *const u32).read_unaligned();
    let mut offset: usize = 8; // skip total_size + reserved

    // Static storage for memory map (up to 128 regions).
    static mut MM: [MemoryRegion; 128] = [MemoryRegion {
        base: 0, length: 0, kind: MemoryType::Reserved
    }; 128];
    let mut mm_idx = 0usize;

    while offset < total_size as usize {
        let tag = (mbi_phys + offset) as *const TagHeader;
        let typ  = (*tag).typ;
        let size = (*tag).size as usize;

        if size < 8 { break; }

        match typ {
            t if t == TagType::End as u32 => break,

            t if t == TagType::MemoryMap as u32 => {
                // Tag layout: header(8) + entry_size(4) + entry_version(4) + entries
                let entry_size = *((mbi_phys + offset + 8) as *const u32) as usize;
                // Guard: entry_size == 0 would make eoff never advance → infinite loop.
                // The spec requires entry_size >= 24; skip the tag if malformed.
                if entry_size == 0 { break; }
                let mut eoff = offset + 16;
                while eoff + entry_size <= offset + size {
                    let e = (mbi_phys + eoff) as *const MbiMemMapEntry;
                    if mm_idx < 128 {
                        MM[mm_idx] = MemoryRegion {
                            base:   (*e).base_addr,
                            length: (*e).length,
                            kind:   match (*e).typ {
                                1 => MemoryType::Available,
                                3 => MemoryType::AcpiReclaimable,
                                4 => MemoryType::AcpiNvs,
                                5 => MemoryType::BadMemory,
                                _ => MemoryType::Reserved,
                            },
                        };
                        mm_idx += 1;
                    }
                    eoff += entry_size;
                }
                info.memory_map     = core::ptr::addr_of!(MM) as *const MemoryRegion;
                info.memory_map_len = mm_idx;
            }

            t if t == TagType::Framebuffer as u32 => {
                let fb = (mbi_phys + offset + 8) as *const MbiFramebuffer;
                info.framebuffer_base   = (*fb).addr;
                info.framebuffer_width  = (*fb).width;
                info.framebuffer_height = (*fb).height;
                info.framebuffer_pitch  = (*fb).pitch;
            }

            t if t == TagType::AcpiV2 as u32 => {
                // ACPI 2.0 RSDP follows the 8-byte tag header.
                info.rsdp_addr = (mbi_phys + offset + 8) as u64;
            }
            t if t == TagType::AcpiV1 as u32 && info.rsdp_addr == 0 => {
                info.rsdp_addr = (mbi_phys + offset + 8) as u64;
            }

            _ => {}
        }

        // Tags are 8-byte aligned.
        offset += (size + 7) & !7;
    }

    info
}
