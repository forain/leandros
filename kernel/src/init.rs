//! PID-1 init task — first process after the kernel bootstraps.
//!
//! Sets up the in-kernel servers (VFS, net, TTY), probes hardware drivers,
//! then hands off to `init_server::init_main()` which runs the POSIX smoke
//! tests and eventually spawns the shell.

use crate::serial_print_str;
use crate::print_hex;
use mm::paging::PageFlags;

extern "C" {
    fn arch_alloc_page_table_root() -> usize;
}

/// The main entry point for the kernel's init task.
pub fn init_task_main(boot_info: &boot::BootInfo) {
    serial_print_str("[INIT] Kernel init task starting\n");

    // ── Userspace Init ───────────────────────────────────────────────────────
    // We attempt to load the 'init' server from the initrd.
    serial_print_str("[INIT] Loading userspace init ELF binary from initrd\n");
    
    let mut actual_initrd_base = boot_info.initrd_base as usize;
    let mut actual_initrd_size = boot_info.initrd_size as usize;

    if actual_initrd_base == 0 {
        serial_print_str("[INIT] No initrd in boot info, trying memory scan...\n");
        if let Some((base, size)) = scan_memory_for_initrd() {
            actual_initrd_base = base;
            actual_initrd_size = size;
        }
    }

    if actual_initrd_base != 0 {
        serial_print_str("[INIT] Found initrd at physical ");
        print_hex(actual_initrd_base);
        serial_print_str(" size ");
        print_hex(actual_initrd_size);
        serial_print_str("\n");

        // Create a temporary BootInfo for extraction
        let tmp_info = boot::BootInfo {
            memory_map: boot_info.memory_map,
            memory_map_len: boot_info.memory_map_len,
            framebuffer_base: boot_info.framebuffer_base,
            framebuffer_width: boot_info.framebuffer_width,
            framebuffer_height: boot_info.framebuffer_height,
            framebuffer_pitch: boot_info.framebuffer_pitch,
            rsdp_addr: boot_info.rsdp_addr,
            uart_base: boot_info.uart_base,
            initrd_base: actual_initrd_base as u64,
            initrd_size: actual_initrd_size as u64,
            hhdm_offset: boot_info.hhdm_offset,
        };

        if let Some(init_elf) = extract_binary_from_initrd("bin/init", &tmp_info) {
            serial_print_str("[INIT] Successfully extracted init binary from initrd\n");
            
            // Register initrd with VFS so it can find files later (like doom1.wad)
            vfs_server::set_initrd(actual_initrd_base, actual_initrd_size);
            // Also register framebuffer with VFS
            vfs_server::set_framebuffer(
                boot_info.framebuffer_base,
                boot_info.framebuffer_width,
                boot_info.framebuffer_height,
                boot_info.framebuffer_pitch,
            );

            // Load and spawn the ELF
            let pid = load_and_spawn_elf(init_elf);
            serial_print_str("[INIT] Userspace init spawned with PID: ");
            crate::print_number(pid);
            serial_print_str("\n");
        } else {
            serial_print_str("[INIT] Error: Could not find bin/init in initrd\n");
            
            // Fallback: try "init" (no bin prefix)
             if let Some(init_elf) = extract_binary_from_initrd("init", &tmp_info) {
                serial_print_str("[INIT] Successfully extracted 'init' (fallback) from initrd\n");
                let pid = load_and_spawn_elf(init_elf);
                serial_print_str("[INIT] Userspace init spawned with PID: ");
                crate::print_number(pid);
                serial_print_str("\n");
             }
        }
    } else {
        serial_print_str("[INIT] Error: No initrd found!\n");
    }

    serial_print_str("[INIT] Starting scheduler loop...\n");
    sched::run();
}

/// Locate a file in the CPIO initrd and return its data.
pub fn extract_binary_from_initrd(name: &str, boot_info: &boot::BootInfo) -> Option<&'static [u8]> {
    let base = boot_info.initrd_base;
    let size = boot_info.initrd_size;

    if base == 0 || size == 0 { return None; }

    let initrd_virt = mm::phys_to_virt(base as usize) as *const u8;
    let initrd_slice = unsafe { core::slice::from_raw_parts(initrd_virt, size as usize) };

    // Diagnostic
    serial_print_str("[CPIO] First 16 bytes of initrd: ");
    for i in 0..16 {
        if i < initrd_slice.len() {
            crate::print_hex(initrd_slice[i] as usize);
            serial_print_str(" ");
        }
    }
    serial_print_str("\n");

    // ── Simple CPIO (newc) parser ───────────────────────────────────────────
    let mut offset = 0;
    let target_name = name.trim_start_matches('/').trim_start_matches("./");

    while offset + 110 <= initrd_slice.len() {
        if &initrd_slice[offset..offset+6] != b"070701" {
            // Check for GZIP magic 1f 8b
            if initrd_slice[offset] == 0x1f && initrd_slice[offset+1] == 0x8b {
                serial_print_str("[CPIO] Found GZIP initrd - extraction NOT SUPPORTED\n");
            } else if offset == 0 {
                serial_print_str("[CPIO] Invalid magic: ");
                serial_print_str(unsafe { core::str::from_utf8_unchecked(&initrd_slice[offset..offset+6]) });
                serial_print_str("\n");
            }
            break;
        }

        let namesize = usize::from_str_radix(core::str::from_utf8(&initrd_slice[offset+94..offset+102]).unwrap_or("0"), 16).unwrap_or(0);
        let filesize = usize::from_str_radix(core::str::from_utf8(&initrd_slice[offset+54..offset+62]).unwrap_or("0"), 16).unwrap_or(0);

        if namesize == 0 { break; }

        let name_start = offset + 110;
        if name_start + namesize > initrd_slice.len() { break; }
        
        let file_name = core::str::from_utf8(&initrd_slice[name_start..name_start + namesize - 1]).unwrap_or("");
        let current_entry_name = file_name.trim_start_matches('/').trim_start_matches("./");
        
        // Align to 4 bytes
        let file_start = (name_start + namesize + 3) & !3;
        
        if current_entry_name == target_name {
            if file_start + filesize > initrd_slice.len() { return None; }
            return Some(unsafe { core::slice::from_raw_parts(initrd_virt.add(file_start), filesize) });
        }

        offset = (file_start + filesize + 3) & !3;
    }

    None
}

/// Helper to load an ELF binary and create a task for it.
fn load_and_spawn_elf(elf_data: &[u8]) -> u32 {
    let root = unsafe { arch_alloc_page_table_root() };
    let mut as_ = mm::vmm::AddressSpace::new(root);
    let entry = elf::load(elf_data, &mut as_).expect("failed to load ELF");
    
    // ── Map userspace stack ─────────────────────────────────────────────────
    // 1 MiB stack ending at 0x0000_1000_0000 (256MB)
    let stack_top = 0x1000_0000usize;
    let stack_size = 0x100000usize;
    let stack_base = stack_top - stack_size;
    let user_sp = stack_top - 64; // Well within mapping and 16-byte aligned
    
    let ok = as_.map(
        stack_base,
        stack_size,
        PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE,
    );
    if !ok { panic!("failed to map userspace stack"); }

    // ── Initialize userspace stack with zeros ───────────────────────────────
    let zero = [0u8; 64];
    if !as_.write_user_buf(user_sp, &zero) { panic!("failed to initialize user stack"); }

    let pid = sched::spawn_user_with_address_space(entry, user_sp, as_).expect("failed to spawn init");
    
    serial_print_str("[INIT] load_and_spawn_elf: entry=0x");
    print_hex(entry);
    serial_print_str(" sp=0x");
    print_hex(user_sp);
    serial_print_str("\n");

    pid
}

/// Scan a range of physical memory for a CPIO signature.
fn scan_memory_for_initrd() -> Option<(usize, usize)> {
    serial_print_str("[INIT-SCAN] Searching for initrd magic (070701)...\n");

    let start: usize;
    let end: usize;

    #[cfg(target_arch = "x86_64")]
    {
        start = 0x01000000;
        end   = 0x20000000;
    }

    #[cfg(target_arch = "aarch64")]
    {
        start = 0x40000000;
        end   = 0x80000000; // Search full 1GB RAM
    }

    let mut ptr = start;
    while ptr + 6 < end {
        let v_ptr = mm::phys_to_virt(ptr) as *const u8;
        unsafe {
            if core::ptr::read_volatile(v_ptr) == b'0' &&
               core::ptr::read_volatile(v_ptr.add(1)) == b'7' &&
               core::ptr::read_volatile(v_ptr.add(2)) == b'0' &&
               core::ptr::read_volatile(v_ptr.add(3)) == b'7' &&
               core::ptr::read_volatile(v_ptr.add(4)) == b'0' &&
               core::ptr::read_volatile(v_ptr.add(5)) == b'1' {
                
                serial_print_str("[INIT-SCAN] Found CPIO magic at physical ");
                print_hex(ptr);
                serial_print_str("\n");
                
                return Some((ptr, 0x2000000)); // Default to 32MB max
            }
        }
        ptr += 4;
    }
    serial_print_str("[INIT-SCAN] No initrd signature found.\n");
    None
}
