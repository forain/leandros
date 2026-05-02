//! PID-1 init task — first process after the kernel bootstraps.
//!
//! Sets up the in-kernel servers (VFS, net, TTY), probes hardware drivers,
//! then hands off to `init_server::init_main()` which runs the POSIX smoke
//! tests and a minimal shell demo before entering the event loop.

use crate::{serial_print, serial_write_raw, serial_read_byte, print_hex, print_number};

/// Kernel-side I/O callbacks passed to the init server library.
static _INIT_IO: init_server::IoHooks = init_server::IoHooks {
    print_str:  |s|   serial_print(s),
    write_raw:  |buf| serial_write_raw(buf),
    read_byte:  ||    serial_read_byte(),
};

// ── Userspace spawn ──────────────────────────────────────────────────────────

/// The main entry point for the kernel-side of the init task.
///
/// This is spawned by the kernel's bootstrap code and becomes PID 1.
pub fn init_task_main(boot_info: &boot::BootInfo) -> ! {
    serial_print("[INIT] Kernel init task starting\n");

    // 1. Initialise in-kernel VFS and net servers.
    if let Some(vfs_port) = vfs_server::init(0) {
        crate::syscall::set_vfs_server_port(vfs_port);
    }

    // 2. Find the initrd.
    serial_print("[INIT] Loading userspace init ELF binary from initrd\n");

    let mut initrd_base = boot_info.initrd_base;
    let mut initrd_size = boot_info.initrd_size;

    if initrd_base == 0 || initrd_size == 0 {
        serial_print("[INIT] No initrd in boot info, trying memory scan...\n");
        if let Some((addr, size)) = scan_memory_for_initrd() {
            initrd_base = addr as u64;
            initrd_size = size as u64;
        }
    }

    if initrd_base == 0 {
        serial_print("[INIT] ERROR: No initrd found! initrd is required.\n");
        serial_print("[INIT] System halted - initrd is mandatory\n");
        loop { core::hint::spin_loop(); }
    }

    serial_print("[INIT] Found initrd at physical ");
    print_hex(initrd_base as usize);
    serial_print(", size ");
    print_hex(initrd_size as usize);
    serial_print("\n");

    // 3. Pass boot information to the VFS server so it can expose initrd and framebuffer.
    vfs_server::set_initrd(initrd_base as usize, initrd_size as usize);
    if boot_info.framebuffer_base != 0 {
        vfs_server::set_framebuffer(
            boot_info.framebuffer_base,
            boot_info.framebuffer_width,
            boot_info.framebuffer_height,
            boot_info.framebuffer_pitch,
        );
    }

    // 4. Extract and spawn the userland init process.
    let initrd_info = boot::BootInfo {
        initrd_base,
        initrd_size,
        ..*boot_info
    };

    match extract_binary_from_initrd("init", &initrd_info) {
        Some(init_binary) => {
            serial_print("[INIT] Successfully extracted init binary from initrd\n");
            match load_and_spawn_elf(init_binary) {
                Some(pid) => {
                    serial_print("[INIT] Userspace init spawned with PID: ");
                    print_number(pid);
                    serial_print("\n");
                }
                None => {
                    serial_print("[INIT] ERROR: Failed to load userspace init ELF\n");
                    panic!("Failed to load init ELF");
                }
            }
        }
        None => {
            serial_print("[INIT] ERROR: Failed to extract init from initrd\n");
            panic!("Could not extract init binary from initrd");
        }
    }

    serial_print("[INIT] Starting scheduler...\n");
    sched::run();
}

/// Locate a file in the CPIO initrd and return its data.
pub fn extract_binary_from_initrd(name: &str, boot_info: &boot::BootInfo) -> Option<&'static [u8]> {
    let base = boot_info.initrd_base;
    let size = boot_info.initrd_size;

    if base == 0 || size == 0 { return None; }

    let initrd_virt = mm::phys_to_virt(base as usize) as *const u8;
    let initrd_slice = unsafe { core::slice::from_raw_parts(initrd_virt, size as usize) };

    // ── Simple CPIO (newc) parser ───────────────────────────────────────────
    let mut offset = 0;
    let target_name = name.trim_start_matches('/').trim_start_matches("./");

    while offset + 110 <= initrd_slice.len() {
        if &initrd_slice[offset..offset+6] != b"070701" {
            break;
        }

        let namesize = usize::from_str_radix(core::str::from_utf8(&initrd_slice[offset+94..offset+102]).unwrap_or("0"), 16).unwrap_or(0);
        let filesize = usize::from_str_radix(core::str::from_utf8(&initrd_slice[offset+54..offset+62]).unwrap_or("0"), 16).unwrap_or(0);

        let name_start = offset + 110;
        if name_start + namesize > initrd_slice.len() { break; }
        
        let file_name = core::str::from_utf8(&initrd_slice[name_start..name_start + namesize - 1]).unwrap_or("");
        let current_entry_name = file_name.trim_start_matches('/').trim_start_matches("./");
        
        // Align to 4 bytes
        let file_start = (name_start + namesize + 3) & !3;
        
        if current_entry_name == target_name {
            if file_start + filesize > initrd_slice.len() { return None; }
            return Some(&initrd_slice[file_start..file_start + filesize]);
        }

        offset = (file_start + filesize + 3) & !3;
        
        if file_name == "TRAILER!!!" { break; }
    }

    None
}

/// Load an ELF binary and spawn it as a new task.
fn load_and_spawn_elf(elf_bytes: &[u8]) -> Option<u32> {
    extern "C" {
        pub fn arch_alloc_page_table_root() -> usize;
    }
    let pt_root = unsafe { arch_alloc_page_table_root() };
    let mut as_ = mm::vmm::AddressSpace::new(pt_root);

    let entry = elf::load(elf_bytes, &mut as_).ok()?;
    serial_print("[INIT] ELF loaded, entry = ");
    print_hex(entry);
    serial_print("\n");
    
    // Initial user stack
    let stack_base = 0x40000000;
    let stack_size = 0x100000; // 1MB
    
    if !as_.map(stack_base, stack_size, mm::paging::PageFlags::USER | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::PRESENT) {
        return None;
    }
    
    // Prefault the top page to ensure it's mapped and zeroed
    as_.prefault_range(stack_base + stack_size - 4096, 4096);
    
    // Set initial SP with enough room to NOT fault past the end of stack
    // System V ABI expects 16-byte alignment
    let sp_top = stack_base + stack_size;
    sched::spawn_user_with_address_space(entry, sp_top - 128, as_)
}

fn scan_memory_for_initrd() -> Option<(usize, usize)> {
    serial_print("[INIT] Scanning memory for initrd signatures...\n");

    // Search broader ranges where QEMU places initrd
    let ranges = [
        (0x1000000, 0x20000000), // 16MB - 512MB
    ];

    for &(start, end) in &ranges {
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
                    
                    serial_print("[INIT] Found CPIO signature at physical ");
                    print_hex(ptr);
                    serial_print("\n");
                    
                    return Some((ptr, 0x1000000)); // Default to 16MB max
                }
            }
            ptr += 0x1000; // Page steps
        }
    }
    None
}
