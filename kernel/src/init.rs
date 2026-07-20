//! PID-1 init task — first process after the kernel bootstraps.
//!
//! Sets up the in-kernel servers (VFS, net, TTY), probes hardware drivers,
//! then hands off to `init_server::init_main()` which runs the POSIX smoke
//! tests and eventually spawns the shell.

use crate::serial_print_str;
use crate::serial_print_hex;
use mm::paging::PageFlags;

extern "C" {
    fn arch_alloc_page_table_root() -> usize;
}

/// The main entry point for the kernel's init task.
pub fn init_task_main(boot_info: &boot::BootInfo) {
    serial_print_str("[INIT] Kernel init task starting\n");

    // Registers the task-exit hook that releases a dying process's IPC reply
    // ports (ipc::port::release_by_owner). Without this, every process that
    // ever calls into a mounted filesystem (which lazily allocates its own
    // reply port on first use) leaks that port forever — the port table's
    // real backing storage is only 64 buckets, so the 64th distinct process
    // to touch a mounted fs permanently exhausts it for the rest of the
    // uptime. See servers/vfs's call_port: a failed port::create() there is
    // treated as silent success (an empty, all-zero reply), so callers like
    // sys_execve's ELF-size lookup see a phantom empty file instead of an
    // error.
    ipc::init();

    // Wire the mm crate's file-backed-VMA hooks to the kernel's exec-file
    // registry (demand-paged exec reads ELF pages in from page faults).
    crate::syscall::init_exec_file_backing();

    // Make every path out of a task — including signal-initiated death, which
    // bypasses the EXIT syscall entirely — release its fds, pipes, sockets and
    // TTY state. See syscall::init_exit_teardown.
    crate::syscall::init_exit_teardown();

    // ── In-Kernel Servers ──────────────────────────────────────────────────
    if let Some(vfs_port) = vfs_server::init(0) {
        crate::syscall::set_vfs_server_port(vfs_port);
        serial_print_str("[INIT] VFS server port: ");
        crate::print_number(vfs_port);
        serial_print_str("\n");
    }
    if let Some(p) = evdev_server::init(0) {
        serial_print_str("[INIT] evdev server port: ");
        crate::print_number(p);
        serial_print_str("\n");
    }
    drivers::virtio_keyboard::init();

    // Initialize DRM server and check for success
    if let Some(p) = drm_server::init(0) {
        serial_print_str("[INIT] DRM server port: ");
        crate::print_number(p);
        serial_print_str("\n");
    } else {
        serial_print_str("[INIT] ERROR: DRM server initialization failed\n");
    }

    // Initialize PipeWire server
    match pipewire_server::init() {
        Ok(p) => {
            serial_print_str("[INIT] PipeWire server port: ");
            crate::print_number(p);
            serial_print_str("\n");
            crate::syscall::set_audio_server_port(p);
        },
        Err(_) => {
            serial_print_str("[INIT] ERROR: PipeWire server initialization failed\n");
        }
    }

    // ── Block Devices & Filesystems ──────────────────────────────────────────
    drivers::blkdev::init();

    // ── USB ───────────────────────────────────────────────────────────────────
    drivers::usb_hcd::init();

    // ── Network Stack ────────────────────────────────────────────────────────
    drivers::virtio_net::init();
    net_server::init();
    sched::spawn(net_server::net_daemon, 0);
    // Block devices are initialized, but f2fs disk mounting is deferred to userspace init.

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

            // Persist the scanned location back into the global BootInfo so
            // later consumers — notably sys_execve, which reads it through
            // BOOT_INFO_PTR — can also find the initrd (e.g. for /bin/shell).
            let bi_ptr = crate::BOOT_INFO_PTR.load(core::sync::atomic::Ordering::SeqCst);
            if bi_ptr != 0 {
                unsafe {
                    let bi = &mut *(bi_ptr as *mut boot::BootInfo);
                    bi.initrd_base = base as u64;
                    bi.initrd_size = size as u64;
                }
            }
        }
    }

    if actual_initrd_base != 0 {
        serial_print_str("[INIT] Found initrd at physical ");
        serial_print_hex(actual_initrd_base);
        serial_print_str(" size ");
        serial_print_hex(actual_initrd_size);
        serial_print_str("\n");

        // Create a temporary BootInfo for extraction
        let tmp_info = boot::BootInfo {
            memory_map:          boot_info.memory_map,
            memory_map_len:      boot_info.memory_map_len,
            framebuffer_base:    boot_info.framebuffer_base,
            framebuffer_width:   boot_info.framebuffer_width,
            framebuffer_height:  boot_info.framebuffer_height,
            framebuffer_pitch:   boot_info.framebuffer_pitch,
            rsdp_addr:           boot_info.rsdp_addr,
            uart_base:           boot_info.uart_base,
            pci_ecam_base:       boot_info.pci_ecam_base,
            // Use the values resolved above (which may come from the memory
            // scan), not the raw boot_info fields — on direct boot the latter
            // are zero and the CPIO parser would see no initrd.
            initrd_base:         actual_initrd_base as u64,
            initrd_size:         actual_initrd_size as u64,
            hhdm_offset:         boot_info.hhdm_offset,
        };


        if let Some(init_elf) = extract_binary_from_initrd("bin/init", &tmp_info) {
            serial_print_str("[INIT] Successfully extracted init binary from initrd\n");
            
            // Register initrd with VFS so it can find files later (like doom1.wad)
            vfs_server::set_initrd(actual_initrd_base, actual_initrd_size);

            // Debug framebuffer and HHDM before registering with VFS
            serial_print_str("[INIT] Framebuffer debug info:\n");
            serial_print_str("[INIT]   Physical base: ");
            crate::serial_print_hex(boot_info.framebuffer_base as usize);
            serial_print_str("\n[INIT]   Resolution: ");
            crate::print_number(boot_info.framebuffer_width);
            serial_print_str("x");
            crate::print_number(boot_info.framebuffer_height);
            serial_print_str("\n[INIT]   Pitch: ");
            crate::print_number(boot_info.framebuffer_pitch);

            // Test virtual address conversion
            let fb_virt = mm::phys_to_virt(boot_info.framebuffer_base as usize);
            serial_print_str("\n[INIT]   Virtual address: ");
            crate::serial_print_hex(fb_virt);

            // Check if virtual address is in valid kernel space
            if fb_virt >= 0xFFFF_0000_0000_0000 {
                serial_print_str("\n[INIT]   Virtual address is in kernel space - OK\n");
            } else {
                serial_print_str("\n[INIT]   WARNING: Virtual address is NOT in kernel space!\n");
            }

            // Registering framebuffer with VFS is now centralized in main.rs
            // to ensure correct pitch heuristics are applied.

            // Debug: Log framebuffer console resolution
            serial_print_str("[INIT] Framebuffer console resolution: ");
            crate::print_number(boot_info.framebuffer_width);
            serial_print_str("x");
            crate::print_number(boot_info.framebuffer_height);
            serial_print_str(" pitch=");
            crate::print_number(boot_info.framebuffer_pitch);
            serial_print_str("\n");

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

/// Walk a CPIO (newc) archive at `base_virt` and return its total length up to
/// and including the `TRAILER!!!` sentinel. Returns 0 if there is no valid CPIO
/// archive at that address. `base_virt` must be a directly-readable address
/// (identity- or HHDM-mapped).
pub fn cpio_image_size(base_virt: usize) -> usize {
    let p = base_virt as *const u8;
    let rd = |o: usize| unsafe { core::ptr::read_volatile(p.add(o)) };
    let parse_hex = |start: usize| -> usize {
        let mut v = 0usize;
        for i in 0..8 {
            let c = rd(start + i);
            let d = match c {
                b'0'..=b'9' => (c - b'0') as usize,
                b'a'..=b'f' => (c - b'a' + 10) as usize,
                b'A'..=b'F' => (c - b'A' + 10) as usize,
                _ => return v,
            };
            v = (v << 4) | d;
        }
        v
    };

    let mut offset = 0usize;
    loop {
        // Validate the newc magic "070701" at the current entry.
        for (i, b) in b"070701".iter().enumerate() {
            if rd(offset + i) != *b { return offset; }
        }
        let namesize = parse_hex(offset + 94);
        let filesize = parse_hex(offset + 54);
        let name_start = offset + 110;

        // The archive ends at the TRAILER!!! entry (which carries no data).
        let trailer = b"TRAILER!!!";
        if namesize >= trailer.len() + 1
            && trailer.iter().enumerate().all(|(i, b)| rd(name_start + i) == *b)
        {
            return (name_start + namesize + 3) & !3;
        }

        let file_start = (name_start + namesize + 3) & !3;
        offset = (file_start + filesize + 3) & !3;
        // Sanity bound: never walk past 1.5 GiB of archive.
        if offset > 0x6000_0000 { return offset; }
    }
}

/// Locate a file in the CPIO initrd and return its data.
pub fn extract_binary_from_initrd(name: &str, boot_info: &boot::BootInfo) -> Option<&'static [u8]> {
    let base = boot_info.initrd_base;
    let size = boot_info.initrd_size;

    if base == 0 || size == 0 { 
        serial_print_str("[CPIO] No initrd available\n");
        return None; 
    }

    let initrd_virt = mm::phys_to_virt(base as usize) as *const u8;
    let initrd_slice = unsafe { core::slice::from_raw_parts(initrd_virt, size as usize) };

    // Make sure the initrd is valid before trying to parse it
    if initrd_slice.len() < 110 {
        serial_print_str("[CPIO] Initrd too small to contain CPIO header\n");
        return None;
    }

    // Diagnostic
    serial_print_str("[CPIO] First 16 bytes of initrd: ");
    for i in 0..16 {
        if i < initrd_slice.len() {
            crate::serial_print_hex(initrd_slice[i] as usize);
            serial_print_str(" ");
        }
    }
    serial_print_str("\n");

    // ── Simple CPIO (newc) parser ───────────────────────────────────────────
    let mut offset = 0;
    let target_name = name.trim_start_matches('/').trim_start_matches("./");
    serial_print_str("[CPIO] Looking for file: ");
    serial_print_str(target_name);
    serial_print_str("\n");

    while offset + 110 <= initrd_slice.len() {
        // Check for valid CPIO header
        if initrd_slice[offset] != b'0' || initrd_slice[offset+1] != b'7' || 
           initrd_slice[offset+2] != b'0' || initrd_slice[offset+3] != b'7' || 
           initrd_slice[offset+4] != b'0' || initrd_slice[offset+5] != b'1' {
            // Check for GZIP magic 1f 8b
            if initrd_slice[offset] == 0x1f && initrd_slice[offset+1] == 0x8b {
                serial_print_str("[CPIO] Found GZIP initrd - extraction NOT SUPPORTED\n");
            } else if offset == 0 {
                serial_print_str("[CPIO] Invalid magic at offset 0: ");
                serial_print_str(unsafe { core::str::from_utf8_unchecked(&initrd_slice[offset..offset+6]) });
                serial_print_str("\n");
            }
            break;
        }

        let namesize = usize::from_str_radix(core::str::from_utf8(&initrd_slice[offset+94..offset+102]).unwrap_or("0"), 16).unwrap_or(0);
        let filesize = usize::from_str_radix(core::str::from_utf8(&initrd_slice[offset+54..offset+62]).unwrap_or("0"), 16).unwrap_or(0);

        if namesize == 0 { 
            serial_print_str("[CPIO] Zero namesize, breaking\n");
            break; 
        }

        let name_start = offset + 110;
        if name_start + namesize > initrd_slice.len() { 
            serial_print_str("[CPIO] Name start + namesize exceeds slice length\n");
            break; 
        }
        
        let file_name = core::str::from_utf8(&initrd_slice[name_start..name_start + namesize - 1]).unwrap_or("");
        let current_entry_name = file_name.trim_start_matches('/').trim_start_matches("./");
        
        // Debug: Print the file name being compared
        serial_print_str("[CPIO] Comparing with file: ");
        serial_print_str(current_entry_name);
        serial_print_str("\n");
        
        // Align to 4 bytes
        let file_start = (name_start + namesize + 3) & !3;
        
        if current_entry_name == target_name {
            if file_start + filesize > initrd_slice.len() { 
                serial_print_str("[CPIO] File data out of bounds\n");
                return None; 
            }
            serial_print_str("[CPIO] Found file in initrd\n");
            return Some(unsafe { core::slice::from_raw_parts(initrd_virt.add(file_start), filesize) });
        }

        offset = (file_start + filesize + 3) & !3;
    }

    serial_print_str("[CPIO] File not found in initrd: ");
    serial_print_str(target_name);
    serial_print_str("\n");
    None
}

/// Helper to load an ELF binary and create a task for it.
fn load_and_spawn_elf(elf_data: &[u8]) -> u32 {
    let root = unsafe { arch_alloc_page_table_root() };
    let mut as_ = mm::vmm::AddressSpace::new(root);
    let elf_info = elf::load(elf_data, &mut as_).expect("failed to load ELF");
    let entry = elf_info.entry;
    
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
    
    serial_print_str("[INIT] load_and_spawn_elf: entry=");
    serial_print_hex(entry);
    serial_print_str(" sp=");
    serial_print_hex(user_sp);
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
        end   = 0xC0000000; // Search full 2GB RAM (-m 2G); QEMU places the
                            // initrd high, above the low 1GB window.
    }

    serial_print_str("[INIT-SCAN] Searching from ");
    serial_print_hex(start);
    serial_print_str(" to ");
    serial_print_hex(end);
    serial_print_str("...\n");

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
                serial_print_hex(ptr);
                serial_print_str("\n");
                
                return Some((ptr, 0x2000000)); // Default to 32MB max
            }
        }
        ptr += 4096;
    }
    serial_print_str("[INIT-SCAN] No initrd signature found.\n");
    None
}
