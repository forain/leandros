//! Flattened Device Tree (DTB) parser — for AArch64 QEMU and real hardware.
//!
//! QEMU -machine virt passes the DTB physical address in x0 on entry.
//! We parse just enough to extract memory regions and the UART base address.
//!
//! Spec: DeviceTree Specification v0.4 §5 (FDT format)

use super::{BootInfo, MemoryRegion, MemoryType};

/// FDT magic number.
pub const FDT_MAGIC: u32 = 0xD00DFEED;

/// FDT header (big-endian on the wire).
#[repr(C)]
struct FdtHeader {
    magic:            u32, // 0xD00DFEED
    totalsize:        u32,
    off_dt_struct:    u32,
    off_dt_strings:   u32,
    off_mem_rsvmap:   u32,
    version:          u32,
    last_comp_version: u32,
    boot_cpuid_phys:  u32,
    size_dt_strings:  u32,
    size_dt_struct:   u32,
}

/// FDT token types.
const FDT_BEGIN_NODE: u32 = 0x00000001;
const FDT_END_NODE:   u32 = 0x00000002;
const FDT_PROP:       u32 = 0x00000003;
const FDT_NOP:        u32 = 0x00000004;
const FDT_END:        u32 = 0x00000009;

fn be32(p: *const u8) -> u32 {
    unsafe {
        u32::from_be_bytes([*p, *p.add(1), *p.add(2), *p.add(3)])
    }
}

fn be64(p: *const u8) -> u64 {
    unsafe {
        u64::from_be_bytes([
            *p, *p.add(1), *p.add(2), *p.add(3),
            *p.add(4), *p.add(5), *p.add(6), *p.add(7),
        ])
    }
}

extern "C" {
    #[allow(dead_code)]
    fn serial_write_byte(b: u8);
}

#[allow(dead_code)]
fn serial_print(msg: &str) {
    for &b in msg.as_bytes() {
        unsafe { serial_write_byte(b); }
    }
}

/// Validate a DTB pointer and return true if it looks like a valid FDT.
///
/// # Safety
/// `dtb_phys` must be a readable physical address.
pub unsafe fn is_valid_dtb(dtb_phys: usize) -> bool {
    if dtb_phys == 0 || dtb_phys & 3 != 0 { return false; }
    // Check for memory access safety - RAM starts at 0x40000000 on virt machine
    if dtb_phys < 0x40000000 || dtb_phys > 0x80000000 { return false; }
    
    // Read the magic directly from physical memory (we are in identity map)
    let magic = be32(dtb_phys as *const u8);
    magic == FDT_MAGIC
}

/// Parse the DTB at `dtb_phys` and populate a `BootInfo`.
///
/// # Safety
/// `dtb_phys` must be the physical address of a valid, complete FDT blob.
pub unsafe fn parse(dtb_phys: usize) -> BootInfo {
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

    if dtb_phys == 0 || !is_valid_dtb(dtb_phys) {
        return info;
    }

    // 64 slots
    static mut MM: [MemoryRegion; 64] = [MemoryRegion {
        base: 0, length: 0, kind: MemoryType::Reserved
    }; 64];
    let mut mm_idx = 0usize;

    let hdr = dtb_phys as *const FdtHeader;
    let struct_off   = be32(core::ptr::addr_of!((*hdr).off_dt_struct)   as *const u8) as usize;
    let strings_off  = be32(core::ptr::addr_of!((*hdr).off_dt_strings)  as *const u8) as usize;
    
    let dt_struct    = (dtb_phys + struct_off) as *const u8;
    let dt_strings   = (dtb_phys + strings_off) as *const u8;

    let mut ptr = dt_struct;
    let mut depth = 0;
    let mut in_chosen = false;
    let mut in_memory = false;
    let mut in_pl011  = false;
    let mut in_framebuf = false;

    let mut address_cells = 2; 
    let mut size_cells = 2;    
    
    let mut initrd_start: u64 = 0;
    let mut initrd_end: u64 = 0;

    loop {
        let token = be32(ptr);
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let name_ptr = ptr.add(4);
                let name_len = strlen(name_ptr);
                let name = core::slice::from_raw_parts(name_ptr, name_len);
                
                in_chosen   = name == b"chosen" || name.starts_with(b"chosen@");
                in_memory   = name.starts_with(b"memory@") || name == b"memory";
                in_pl011    = name.starts_with(b"pl011@") || name.starts_with(b"uart@");
                in_framebuf = name.starts_with(b"framebuffer@") || name == b"framebuffer";

                ptr = ptr.add(4 + align_up(name_len + 1, 4));
            }
            FDT_END_NODE => {
                depth -= 1;
                if in_chosen {
                    info.initrd_base = initrd_start;
                    if initrd_end > initrd_start {
                        info.initrd_size = initrd_end - initrd_start;
                    }
                }
                in_chosen = false;
                in_memory = false;
                in_pl011 = false;
                in_framebuf = false;
                if depth == 0 { break; }
                ptr = ptr.add(4);
            }
            FDT_PROP => {
                let data_len = be32(ptr.add(4)) as usize;
                let name_off = be32(ptr.add(8)) as usize;
                let name_ptr = dt_strings.add(name_off);
                let pn_len   = strlen(name_ptr);
                let data_ptr = ptr.add(12);
                let prop_name = core::slice::from_raw_parts(name_ptr, pn_len);

                match prop_name {
                    b"#address-cells" if depth == 1 => { address_cells = be32(data_ptr) as u8; }
                    b"#size-cells" if depth == 1 => { size_cells = be32(data_ptr) as u8; }
                    
                    b"reg" if in_framebuf && data_len >= 8 => {
                        info.framebuffer_base = if address_cells >= 2 { be64(data_ptr) } else { be32(data_ptr) as u64 };
                    }
                    b"width" if in_framebuf && data_len >= 4 => { info.framebuffer_width = be32(data_ptr); }
                    b"height" if in_framebuf && data_len >= 4 => { info.framebuffer_height = be32(data_ptr); }
                    b"stride" if in_framebuf && data_len >= 4 => { info.framebuffer_pitch = be32(data_ptr); }

                    b"reg" if in_pl011 && data_len >= 4 => {
                        info.uart_base = if address_cells >= 2 { be64(data_ptr) } else { be32(data_ptr) as u64 };
                    }

                    b"reg" if in_memory && mm_idx < 60 => {
                        let entry_bytes = (address_cells + size_cells) as usize * 4;
                        if entry_bytes > 0 {
                            let mut off = 0;
                            while off + entry_bytes <= data_len {
                                let base = if address_cells == 2 { be64(data_ptr.add(off)) } else { be32(data_ptr.add(off)) as u64 };
                                let size_off = off + address_cells as usize * 4;
                                let size = if size_cells == 2 { be64(data_ptr.add(size_off)) } else { be32(data_ptr.add(size_off)) as u64 };
                                unsafe {
                                    MM[mm_idx] = MemoryRegion { base, length: size, kind: MemoryType::Available };
                                    mm_idx += 1;
                                }
                                off += entry_bytes;
                            }
                        }
                    }

                    b"linux,initrd-start" if in_chosen && data_len >= 4 => {
                        initrd_start = if data_len >= 8 { be64(data_ptr) } else { be32(data_ptr) as u64 };
                    }
                    b"linux,initrd-end" if in_chosen && data_len >= 4 => {
                        initrd_end = if data_len >= 8 { be64(data_ptr) } else { be32(data_ptr) as u64 };
                    }
                    _ => {}
                }
                ptr = ptr.add(12 + align_up(data_len, 4));
            }
            FDT_NOP => { ptr = ptr.add(4); }
            FDT_END => { break; }
            _ => { break; }
        }
    }

    info.memory_map = core::ptr::addr_of!(MM) as *const MemoryRegion;
    info.memory_map_len = mm_idx;
    info
}

fn strlen(s: *const u8) -> usize {
    let mut len = 0;
    unsafe { while *s.add(len) != 0 { len += 1; } }
    len
}

fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}
