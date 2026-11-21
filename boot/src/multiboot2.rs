//! Multiboot2 boot information parser — from multiboot2 spec §3.6
//!
//! GRUB (or QEMU -kernel) passes a pointer to the MBI in EBX (32-bit) /
//! RSI (64-bit after our trampoline).  We parse it here into our BootInfo.

use super::{stash_cmdline, BootInfo, MemoryRegion, MemoryType};

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

/// LeandrOS private tag (kexec): the previous kernel's final log buffer,
/// payload `phys: u64, size: u64` (`boot::kexec_prevlog`). Unknown to every
/// other loader and kernel, which skip it.
pub const TAG_LEANDROS_PREVLOG: u32 = 0x4C4F_5301;

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

    // MBI starts with total_size (u32) + reserved (u32), then tags.
    let total_size = (mbi_phys as *const u32).read_unaligned();
    let mut offset: usize = 8; // skip total_size + reserved

    // Static storage for memory map (up to 128 regions).
    static mut MM: [MemoryRegion; 128] = [MemoryRegion {
        base: 0, length: 0, kind: MemoryType::Reserved
    }; 128];
    let mut mm_idx = 0usize;
    // EFI memory map tag (17), kept for after the loop: used only when the
    // firmware sent no tag-6 map.
    let mut efi_mm: Option<(usize, usize)> = None;

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

            t if t == TagType::BootCmdLine as u32 && size > 8 => {
                // NUL-terminated string after the header.
                let p = (mbi_phys + offset + 8) as *const u8;
                stash_cmdline(&mut info, core::slice::from_raw_parts(p, size - 8));
            }

            t if t == TagType::Module as u32 && size >= 16 && info.initrd_base == 0 => {
                // header(8) + mod_start(4) + mod_end(4) + string; the first
                // module is the initrd.
                let start = *((mbi_phys + offset + 8) as *const u32) as u64;
                let end   = *((mbi_phys + offset + 12) as *const u32) as u64;
                if end > start {
                    info.initrd_base = start;
                    info.initrd_size = end - start;
                }
            }

            t if t == TagType::Efi64 as u32 && size >= 16 => {
                info.efi_system_table = *((mbi_phys + offset + 8) as *const u64);
            }

            t if t == TagType::EfiMemMap as u32 && size >= 16 => {
                efi_mm = Some((offset, size));
            }

            TAG_LEANDROS_PREVLOG if size >= 24 => {
                let p = ((mbi_phys + offset + 8) as *const u64).read_unaligned();
                let l = ((mbi_phys + offset + 16) as *const u64).read_unaligned();
                super::set_kexec_prevlog(p, l);
            }

            _ => {}
        }

        // Tags are 8-byte aligned.
        offset += (size + 7) & !7;
    }

    if info.memory_map_len == 0 {
        if let Some((offset, size)) = efi_mm {
            // header(8) + descriptor_size(4) + descriptor_version(4) + descriptors.
            // Descriptor: type u32, pad u32, phys_start u64, virt_start u64,
            // num_pages u64, attribute u64 (the stride is descriptor_size).
            let dsize = *((mbi_phys + offset + 8) as *const u32) as usize;
            if dsize >= 32 {
                let mut eoff = offset + 16;
                while eoff + dsize <= offset + size && mm_idx < 128 {
                    let d = (mbi_phys + eoff) as *const u8;
                    let typ   = (d as *const u32).read_unaligned();
                    let start = (d.add(8)  as *const u64).read_unaligned();
                    let pages = (d.add(24) as *const u64).read_unaligned();
                    MM[mm_idx] = MemoryRegion {
                        base:   start,
                        length: pages * 4096,
                        kind:   match typ {
                            // Loader and boot-services code/data, conventional RAM.
                            1 | 2 | 3 | 4 | 7 => MemoryType::Available,
                            9  => MemoryType::AcpiReclaimable,
                            10 => MemoryType::AcpiNvs,
                            _  => MemoryType::Reserved,
                        },
                    };
                    mm_idx += 1;
                    eoff += dsize;
                }
                info.memory_map     = core::ptr::addr_of!(MM) as *const MemoryRegion;
                info.memory_map_len = mm_idx;
            }
        }
    }

    info
}

/// Xen PVH `hvm_start_info` magic (`xen/include/public/arch-x86/hvm/start_info.h`).
pub const PVH_START_INFO_MAGIC: u32 = 0x336e_c578;

/// E820-style type (multiboot1 mmap, PVH memmap, multiboot2 tag 6) to ours.
fn e820_kind(t: u32) -> MemoryType {
    match t {
        1 => MemoryType::Available,
        3 => MemoryType::AcpiReclaimable,
        4 => MemoryType::AcpiNvs,
        5 => MemoryType::BadMemory,
        _ => MemoryType::Reserved,
    }
}

/// Memory map storage for the multiboot1 / PVH parsers (one of them runs).
static mut LEGACY_MM: [MemoryRegion; 128] = [MemoryRegion {
    base: 0, length: 0, kind: MemoryType::Reserved
}; 128];

/// Parse a PVH start_info block at `si_phys` (QEMU `-kernel` on an ELF with a
/// PVH note: `-append` becomes `cmdline_paddr`, `-initrd` the first module).
/// The command line, initrd, RSDP and (start_info version >= 1) the memory
/// map are taken.
///
/// # Safety
/// `si_phys` must be readable and start with `PVH_START_INFO_MAGIC`.
pub unsafe fn parse_pvh(si_phys: usize) -> BootInfo {
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
    // magic u32, version u32, flags u32, nr_modules u32, modlist_paddr u64,
    // cmdline_paddr u64, rsdp_paddr u64
    let rd32 = |o: usize| ((si_phys + o) as *const u32).read_unaligned();
    let rd64 = |o: usize| ((si_phys + o) as *const u64).read_unaligned();
    let nr_modules = rd32(12);
    let modlist    = rd64(16) as usize;
    let cmdline    = rd64(24) as usize;
    info.rsdp_addr = rd64(32);
    if cmdline != 0 {
        let p = cmdline as *const u8;
        let mut n = 0;
        while n < super::CMDLINE_SCAN_MAX && *p.add(n) != 0 { n += 1; }
        stash_cmdline(&mut info, core::slice::from_raw_parts(p, n));
    }
    // version >= 1: memmap_paddr u64 @40, memmap_entries u32 @48;
    // hvm_memmap_table_entry: addr u64, size u64, type u32, reserved u32.
    if rd32(4) >= 1 {
        let mm = rd64(40) as usize;
        let n = (rd32(48) as usize).min(128);
        if mm != 0 {
            let out = &mut *core::ptr::addr_of_mut!(LEGACY_MM);
            for i in 0..n {
                let e = mm + i * 24;
                out[i] = MemoryRegion {
                    base: (e as *const u64).read_unaligned(),
                    length: ((e + 8) as *const u64).read_unaligned(),
                    kind: e820_kind(((e + 16) as *const u32).read_unaligned()),
                };
            }
            info.memory_map = out.as_ptr();
            info.memory_map_len = n;
        }
    }
    if nr_modules > 0 && modlist != 0 {
        // hvm_modlist_entry: paddr u64, size u64, cmdline_paddr u64, reserved u64
        info.initrd_base = (modlist as *const u64).read_unaligned();
        info.initrd_size = ((modlist + 8) as *const u64).read_unaligned();
    }
    info
}

/// Multiboot1 bootloader magic (EAX): what QEMU `-kernel` hands an ELF that
/// carries a multiboot header, ahead of its PVH note.
pub const MULTIBOOT1_BOOTLOADER_MAGIC: u32 = 0x2BAD_B002;

/// Parse a multiboot1 info block at `mbi_phys`: command line (flag bit 2) and
/// the first module (bit 3) as the initrd, and the memory map (bit 6).
///
/// # Safety
/// `mbi_phys` must be a valid multiboot1 info structure.
pub unsafe fn parse_multiboot1(mbi_phys: usize) -> BootInfo {
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
    let rd32 = |a: usize| (a as *const u32).read_unaligned();
    let flags = rd32(mbi_phys);
    if flags & (1 << 2) != 0 {
        let c = rd32(mbi_phys + 16) as usize;
        if c != 0 {
            let p = c as *const u8;
            let mut n = 0;
            while n < super::CMDLINE_SCAN_MAX && *p.add(n) != 0 { n += 1; }
            stash_cmdline(&mut info, core::slice::from_raw_parts(p, n));
        }
    }
    // Bit 6: mmap_length @44, mmap_addr @48; entries are
    // { size u32 (excluding itself), base u64, length u64, type u32 }.
    if flags & (1 << 6) != 0 {
        let len = rd32(mbi_phys + 44) as usize;
        let addr = rd32(mbi_phys + 48) as usize;
        let out = &mut *core::ptr::addr_of_mut!(LEGACY_MM);
        let (mut off, mut n) = (0usize, 0usize);
        while addr != 0 && off + 24 <= len && n < 128 {
            let e = addr + off;
            let size = rd32(e) as usize;
            if size < 20 { break; }
            out[n] = MemoryRegion {
                base: ((e + 4) as *const u64).read_unaligned(),
                length: ((e + 12) as *const u64).read_unaligned(),
                kind: e820_kind(rd32(e + 20)),
            };
            n += 1;
            off += size + 4;
        }
        if n > 0 {
            info.memory_map = out.as_ptr();
            info.memory_map_len = n;
        }
    }
    if flags & (1 << 3) != 0 && rd32(mbi_phys + 20) > 0 {
        // mods_addr -> { mod_start, mod_end, string, reserved }
        let m = rd32(mbi_phys + 24) as usize;
        let (start, end) = (rd32(m) as u64, rd32(m + 4) as u64);
        if m != 0 && end > start {
            info.initrd_base = start;
            info.initrd_size = end - start;
        }
    }
    info
}
