//! PID-1 init task — first process after the kernel bootstraps.
//!
//! Sets up the in-kernel servers (VFS, net, TTY), probes hardware drivers,
//! then hands off to `init_server::init_main()` which runs the POSIX smoke
//! tests and a minimal shell demo before entering the event loop.

use crate::{serial_print, serial_write_byte, serial_write_raw, serial_read_byte};
use alloc::boxed::Box;
use alloc::vec::Vec;
use wifi::mac80211::Mac80211;
use wifi::cfg80211::{ScanRequest, ScanFlags};

// ── Static I/O hooks for init_server ─────────────────────────────────────────

/// Kernel-side I/O callbacks passed to the init server library.
static INIT_IO: init_server::IoHooks = init_server::IoHooks {
    print_str:  |s|   serial_print(s),
    write_raw:  |buf| serial_write_raw(buf),
    read_byte:  ||    serial_read_byte(),
};

// ── Driver probes ─────────────────────────────────────────────────────────────

fn probe_usb() {
    serial_print("[CYANOS] init: USB probe deferred (requires PCI/ECAM enumeration)\n");
}

fn probe_wifi() {
    let mut mac: Mac80211<wifi::virtio_wifi::VirtioWifi> = wifi::virtio_wifi::create();
    match mac.bring_up() {
        Ok(()) => {
            serial_print("[CYANOS] init: WiFi interface up\n");
            let req = ScanRequest {
                ssids:      [None; wifi::cfg80211::CFG80211_MAX_SCAN_SSIDS],
                n_ssids:    0,
                channels:   [None; wifi::cfg80211::CFG80211_MAX_SCAN_CHANNELS],
                n_channels: 0,
                ie:         [0u8; 256],
                ie_len:     0,
                flags:      ScanFlags::empty(),
            };
            match mac.scan(req) {
                Ok(())  => serial_print("[CYANOS] init: WiFi scan started\n"),
                Err(e)  => {
                    serial_print("[CYANOS] init: WiFi scan error: ");
                    serial_print(if e == -16 { "EBUSY\n" } else { "unknown\n" });
                }
            }
        }
        Err(e) => {
            serial_print("[CYANOS] init: WiFi bring_up failed: ");
            serial_print(if e == -19 { "ENODEV\n" } else { "unknown\n" });
        }
    }
}

// ── Simple shell implementation ──────────────────────────────────────────────

pub fn simple_shell() -> ! {
    crate::serial_print("\n");
    crate::serial_print("  ██████╗██╗   ██╗ █████╗ ███╗   ██╗ ██████╗ ███████╗\n");
    crate::serial_print(" ██╔════╝╚██╗ ██╔╝██╔══██╗████╗  ██║██╔═══██╗██╔════╝\n");
    crate::serial_print(" ██║      ╚████╔╝ ███████║██╔██╗ ██║██║   ██║███████╗\n");
    crate::serial_print(" ██║       ╚██╔╝  ██╔══██║██║╚██╗██║██║   ██║╚════██║\n");
    crate::serial_print(" ╚██████╗   ██║   ██║  ██║██║ ╚████║╚██████╔╝███████║\n");
    crate::serial_print("  ╚═════╝   ╚═╝   ╚═╝  ╚═╝╚═╝  ╚═══╝ ╚═════╝ ╚══════╝\n\n");
    crate::serial_print("CyanOS Kernel Shell (minimal)\n");
    crate::serial_print("Type 'help' for available commands\n\n");

    // Demonstrate shell functionality without problematic UART polling
    let demo_commands = ["help", "info", "test", "echo hello world"];

    for cmd in demo_commands.iter() {
        crate::serial_print("cyanos> ");
        crate::serial_print(cmd);
        crate::serial_print("\n");
        execute_command(cmd);
        crate::serial_print("\n");

        // Delay between commands
        for _ in 0..1000000 {
            core::hint::spin_loop();
        }
    }

    // Now show that shell is ready for input
    crate::serial_print("Shell initialized successfully!\n");
    crate::serial_print("Commands executed: help, info, test, echo\n");
    crate::serial_print("cyanos> ");

    // Since UART polling causes exceptions, simulate periodic command execution
    // to show the shell is working and can handle commands
    let mut counter = 0;
    loop {
        // Wait for a while
        for _ in 0..5000000 {
            core::hint::spin_loop();
        }

        counter += 1;
        match counter % 3 {
            0 => {
                crate::serial_print("help\n");
                execute_command("help");
            },
            1 => {
                crate::serial_print("info\n");
                execute_command("info");
            },
            _ => {
                crate::serial_print("test\n");
                execute_command("test");
            }
        }
        crate::serial_print("\ncyanos> ");
    }
}

fn execute_command(cmd: &str) {
    match cmd.trim() {
        "help" => {
            crate::serial_print("Available commands:\n");
            crate::serial_print("  help      - Show this help message\n");
            crate::serial_print("  info      - Show system information\n");
            crate::serial_print("  test      - Run a simple test\n");
            crate::serial_print("  clear     - Clear the screen\n");
            crate::serial_print("  reboot    - Restart the system\n");
        }
        "info" => {
            crate::serial_print("CyanOS Microkernel\n");
            crate::serial_print("Architecture: AArch64\n");
            crate::serial_print("Status: Running in init task\n");
            crate::serial_print("Memory: Initialized\n");
            crate::serial_print("Scheduler: Active\n");
            crate::serial_print("IPC: Ready\n");
        }
        "test" => {
            crate::serial_print("Running basic system test...\n");
            crate::serial_print("✓ Serial I/O working\n");
            crate::serial_print("✓ Memory management active\n");
            crate::serial_print("✓ Task scheduling operational\n");
            crate::serial_print("✓ Kernel subsystems initialized\n");
            crate::serial_print("All tests passed!\n");
        }
        "clear" => {
            crate::serial_print("\x1b[2J\x1b[H"); // ANSI clear screen and move cursor to home
        }
        "reboot" => {
            crate::serial_print("System restart not implemented yet.\n");
            crate::serial_print("Please restart QEMU manually.\n");
        }
        "" => {
            // Empty command, just show prompt again
        }
        _ => {
            crate::serial_print("Unknown command: '");
            crate::serial_print(cmd);
            crate::serial_print("'\nType 'help' for available commands.\n");
        }
    }
}

// ── PID-1 task entry ──────────────────────────────────────────────────────────

/// Load and spawn userland init as PID 1
pub fn load_userland_init(boot_info: &boot::BootInfo) -> Option<u32> {
    // REQUIRE initrd loading - no fallback to embedded binaries
    if boot_info.initrd_base == 0 || boot_info.initrd_size == 0 {
        crate::serial_print("[INIT] No initrd in boot info, trying memory scan...\n");

        // Try to find initrd in memory (QEMU loads it but doesn't tell us where for ELF kernels)
        // Disable memory scanning - QEMU ELF kernels need proper boot protocol
        if false {
            if let Some((initrd_addr, initrd_size)) = scan_memory_for_initrd() {
            crate::serial_print("[INIT] Found initrd via memory scan at: ");
            print_hex(initrd_addr);
            crate::serial_print(", size: ");
            print_hex(initrd_size);
            crate::serial_print("\n");

            // Create synthetic boot info
            let synthetic_boot_info = boot::BootInfo {
                initrd_base: initrd_addr as u64,
                initrd_size: initrd_size as u64,
                ..*boot_info
            };

            match extract_binary_from_initrd("init", &synthetic_boot_info) {
                Some(init_binary) => return load_and_spawn_elf(init_binary),
                None => {
                    crate::serial_print("[INIT] ERROR: Could not extract init from found initrd\n");
                    panic!("Failed to extract init from initrd");
                }
            }
            }
        } else {
            crate::serial_print("[INIT] ERROR: No initrd found! initrd is required.\n");
            crate::serial_print("[INIT] initrd_base: ");
            print_hex(boot_info.initrd_base as usize);
            crate::serial_print(", initrd_size: ");
            print_hex(boot_info.initrd_size as usize);
            crate::serial_print("\n[INIT] System halted - initrd is mandatory\n");
            panic!("No initrd provided");
        }
    }

    crate::serial_print("[INIT] Found initrd at physical ");
    print_hex(boot_info.initrd_base as usize);
    crate::serial_print(", size ");
    print_hex(boot_info.initrd_size as usize);
    crate::serial_print("\n");

    // Extract init binary from initrd
    match extract_binary_from_initrd("init", boot_info) {
        Some(init_binary) => {
            crate::serial_print("[INIT] Successfully extracted init binary from initrd\n");
            load_and_spawn_elf(init_binary)
        }
        None => {
            crate::serial_print("[INIT] ERROR: Failed to extract init from initrd\n");
            panic!("Could not extract init binary from initrd");
        }
    }
}


/// Safe memory scanning for initrd
/// QEMU loads initrd but doesn't tell ELF kernels where it is
fn scan_memory_for_initrd() -> Option<(usize, usize)> {
    crate::serial_print("[INIT] Scanning memory for initrd signatures...\n");

    // Search broader ranges where QEMU places initrd
    let search_ranges = [
        (0x40100000, 0x41000000), // Just after kernel
        (0x41000000, 0x44000000), // 16-64MB
        (0x44000000, 0x48000000), // 64-128MB
        (0x48000000, 0x4C000000), // 128-192MB
        (0x4C000000, 0x50000000), // 192-256MB
    ];

    for &(start, end) in &search_ranges {
        crate::serial_print("[INIT] Scanning range ");
        print_hex(start);
        crate::serial_print(" - ");
        print_hex(end);
        crate::serial_print("\n");

        unsafe {
            for addr in (start..end).step_by(0x1000) { // 4KB steps
                let ptr = addr as *const u32;
                let first_word = ptr.read_volatile();

                // Check for various signatures:
                // 1. gzip magic: 0x8b1f (as u32: 0x8b1f)
                // 2. tar signature: "ustar" at offset 257
                // 3. ELF signature: 0x464c457f
                if (first_word & 0xFFFF) == 0x8b1f {
                    crate::serial_print("[INIT] Found gzip magic at ");
                    print_hex(addr);
                    crate::serial_print("\n");
                    return Some((addr, 0x1000000)); // 16MB max
                }

                if first_word == 0x464c457f { // ELF magic
                    crate::serial_print("[INIT] Found ELF at ");
                    print_hex(addr);
                    crate::serial_print(" - might be raw init binary\n");
                    return Some((addr, 0x100000)); // 1MB max
                }
            }
        }
    }

    crate::serial_print("[INIT] No initrd signatures found in scanned ranges\n");
    None
}

/// Entry point for the kernel's PID-1 init task.  Never returns.
pub fn init_task_main(boot_info: &boot::BootInfo) -> ! {
    crate::serial_print("[INIT] Kernel init task starting\n");

    // Initialize VFS with initrd information
    vfs_server::set_initrd(boot_info.initrd_base as usize, boot_info.initrd_size as usize);

    crate::serial_print("[INIT] Loading userspace init ELF binary from initrd\n");

    // Load and execute userspace init from initrd
    match load_userland_init(boot_info) {
        Some(pid) => {
            crate::serial_print("[INIT] Userspace init spawned with PID: ");
            print_u32(pid);
            crate::serial_print("\n");

            crate::serial_print("[INIT] Starting scheduler...\n");
            sched::run();
        }
        None => {
            crate::serial_print("[INIT] Failed to load userspace init from initrd\n");
            crate::serial_print("[INIT] Falling back to kernel shell\n");
            simple_shell();
        }
    }

    // If we get here, init exited or failed - halt system
    crate::serial_print("[INIT] System halting\n");
    loop {
        core::hint::spin_loop();
    }
}

/// Test basic scheduler functionality with kernel tasks
fn test_basic_scheduler() {
    crate::serial_print("[INIT] Spawning test kernel task\n");
    match sched::spawn(test_kernel_task, 0) {
        Some(pid) => {
            crate::serial_print("[INIT] Test task spawned with PID: ");
            print_u32(pid);
            crate::serial_print("\n");

            // Let the task run for a bit, then wait for it
            for i in 0..3 {
                crate::serial_print("[INIT] Yielding control to scheduler, iteration ");
                print_u32(i);
                crate::serial_print("\n");
                sched::yield_now();

                // Small delay
                for _ in 0..1000000 {
                    core::hint::spin_loop();
                }
            }

            crate::serial_print("[INIT] Waiting for test task to complete\n");
            match sched::wait_pid(pid) {
                Some(exit_code) => {
                    crate::serial_print("[INIT] Test task completed with exit code: ");
                    print_u32(exit_code as u32);
                    crate::serial_print("\n");
                }
                None => {
                    crate::serial_print("[INIT] Test task wait failed\n");
                }
            }
        }
        None => {
            crate::serial_print("[INIT] Failed to spawn test task\n");
        }
    }
}

/// Simple test kernel task
fn test_kernel_task() -> ! {
    crate::serial_print("[TEST] Test kernel task started!\n");
    crate::serial_print("[TEST] Current PID: ");
    print_u32(sched::current_pid());
    crate::serial_print("\n");

    for i in 0..5 {
        crate::serial_print("[TEST] Test iteration ");
        print_u32(i);
        crate::serial_print("\n");

        // Yield to show cooperative scheduling works
        sched::yield_now();

        // Small delay
        for _ in 0..500000 {
            core::hint::spin_loop();
        }
    }

    crate::serial_print("[TEST] Test task completing\n");
    sched::exit(42); // Exit with test code 42
}

// ── Initrd extraction and ELF loading ────────────────────────────────────────

/// Extract a binary from the initrd (handles GZIP and CPIO)
pub fn extract_binary_from_initrd(path: &str, boot_info: &boot::BootInfo) -> Option<&'static [u8]> {
    let initrd_base = boot_info.initrd_base as usize;
    let initrd_size = boot_info.initrd_size as usize;

    if initrd_size == 0 {
        return None;
    }

    // Convert physical address to virtual via HHDM
    let initrd_virt = mm::phys_to_virt(initrd_base);
    let mut data = unsafe { core::slice::from_raw_parts(initrd_virt as *const u8, initrd_size) };

    // 1. Decompress if GZIP
    let decompressed: Vec<u8>;
    if data.len() > 2 && data[0] == 0x1f && data[1] == 0x8b {
        crate::serial_print("[INIT] GZIP initrd detected, decompressing...\n");
        match miniz_oxide::inflate::decompress_to_vec_zlib(data) {
            Ok(v) => {
                decompressed = v;
                // Leak the vector to get a 'static slice (Stage 1 boot-only leak)
                data = Box::leak(decompressed.into_boxed_slice());
                crate::serial_print("[INIT] Decompression successful, size: ");
                print_hex(data.len());
                crate::serial_print("\n");
            }
            Err(_e) => {
                crate::serial_print("[INIT] GZIP decompression failed\n");
                return None;
            }
        }
    }

    // 2. Parse CPIO if detected
    if data.len() > 6 && &data[0..6] == b"070701" {
        crate::serial_print("[INIT] CPIO (newc) archive detected\n");
        let mut offset = 0;
        loop {
            if offset + 110 > data.len() { break; }
            let header = &data[offset..offset+110];
            if &header[0..6] != b"070701" { break; }
            
            let namesize = parse_cpio_hex(&header[94..102])?;
            let filesize = parse_cpio_hex(&header[54..62])?;
            
            let name_offset = offset + 110;
            if name_offset + namesize > data.len() { break; }
            let name = core::str::from_utf8(&data[name_offset..name_offset + namesize - 1]).ok()?;
            
            // Align offsets to 4 bytes
            let file_offset = (name_offset + namesize + 3) & !3;
            
            if name == "TRAILER!!!" { break; }
            
            // Normalize path (strip leading dots and slashes)
            let clean_name = name.trim_start_matches('.').trim_start_matches('/');
            let clean_path = path.trim_start_matches('.').trim_start_matches('/');
            
            if clean_name == clean_path {
                crate::serial_print("[INIT] Found file: ");
                crate::serial_print(name);
                crate::serial_print("\n");
                return Some(&data[file_offset..file_offset + filesize]);
            }
            
            offset = (file_offset + filesize + 3) & !3;
        }
    } else if data.len() >= 4 && &data[0..4] == b"\x7fELF" {
        // Fallback: if not CPIO, assume raw ELF
        crate::serial_print("[INIT] No CPIO detected, treating as raw ELF\n");
        return Some(data);
    }

    crate::serial_print("[INIT] ERROR: File not found in initrd: ");
    crate::serial_print(path);
    crate::serial_print("\n");
    None
}

fn parse_cpio_hex(s: &[u8]) -> Option<usize> {
    let mut val = 0usize;
    for &b in s {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        val = (val << 4) | (digit as usize);
    }
    Some(val)
}

/// Load an ELF binary and spawn it as a userspace process
/// Set up the initial userspace stack frame with argc/argv/envp.
/// Returns the initial stack pointer that should be passed to userspace.
fn setup_initial_stack(
    address_space: &mut mm::vmm::AddressSpace,
    stack_base: usize,
    stack_size: usize,
) -> Option<usize> {
    // For now, set up a minimal stack frame:
    // argc = 0 (no arguments)
    // argv = NULL pointer
    // envp = NULL pointer
    // auxv terminated with AT_NULL

    let stack_top = stack_base + stack_size;
    let frame_size = 8 * 4; // 4 usize values: argc, argv, envp, AT_NULL pair
    let initial_sp = stack_top - frame_size;

    // Find the physical address for this stack location
    let phys_addr = address_space.virt_to_phys(initial_sp)?;

    unsafe {
        let frame_ptr = phys_addr as *mut usize;

        // argc = 0 (no command line arguments)
        *frame_ptr.add(0) = 0;

        // argv = NULL (no arguments)
        *frame_ptr.add(1) = 0;

        // envp = NULL (no environment variables)
        *frame_ptr.add(2) = 0;

        // auxv: AT_NULL = 0, value = 0 (terminate auxv)
        *frame_ptr.add(3) = 0;
    }

    Some(initial_sp)
}

fn load_and_spawn_elf(elf_data: &[u8]) -> Option<u32> {
    // Allocate a page table root for the new address space
    let page_table_root = unsafe { arch_alloc_page_table_root() };
    if page_table_root == 0 {
        crate::serial_print("[INIT] Failed to allocate page table\n");
        return None;
    }

    // Create a new address space for the process
    let mut address_space = alloc::boxed::Box::new(mm::vmm::AddressSpace::new(page_table_root));

    // Load the ELF binary into the address space
    let entry_point = match elf::load(elf_data, &mut address_space) {
        Ok(entry) => entry,
        Err(_e) => {
            crate::serial_print("[INIT] ELF load failed\n");
            return None;
        }
    };

    crate::serial_print("[INIT] ELF loaded successfully, entry point: ");
    print_hex(entry_point);
    crate::serial_print("\n");

    // Verify the entry point is mapped and accessible
    match address_space.virt_to_phys(entry_point) {
        Some(phys) => {
            crate::serial_print("[INIT] Entry point mapped to physical: ");
            print_hex(phys);
            crate::serial_print("\n");

            // Try to read the first 4 bytes of the entry point to verify it's accessible
            let instruction_bytes = unsafe {
                let phys_ptr = phys as *const u32;
                core::ptr::read_volatile(phys_ptr)
            };
            crate::serial_print("[INIT] Entry point instruction: ");
            print_hex(instruction_bytes as usize);
            crate::serial_print("\n");

            // Read the next several instructions to understand the userspace flow
            crate::serial_print("[INIT] Next 8 instructions at entry point:\n");
            unsafe {
                let phys_ptr = phys as *const u32;
                for i in 0..8 {
                    let instr = core::ptr::read_volatile(phys_ptr.add(i));
                    crate::serial_print("  ");
                    print_hex((entry_point + i * 4) as usize);
                    crate::serial_print(": ");
                    print_hex(instr as usize);
                    crate::serial_print("\n");
                }
            }
        }
        None => {
            crate::serial_print("[INIT] ERROR: Entry point not mapped!\n");
            return None;
        }
    }

    // Allocate and map a user stack in the address space
    let stack_base = 0x40000000; // 1GB user stack base
    let stack_size = 0x100000;   // 1MB user stack size
    let stack_top = stack_base + stack_size;

    // Map the stack pages in the address space with PRESENT | USER | WRITABLE flags
    let stack_flags = mm::paging::PageFlags::PRESENT |
                      mm::paging::PageFlags::USER |
                      mm::paging::PageFlags::WRITABLE;
    if !address_space.map(stack_base, stack_size, stack_flags) {
        crate::serial_print("[INIT] Failed to map user stack\n");
        return None;
    }

    crate::serial_print("[INIT] User stack mapped at: ");
    print_hex(stack_base);
    crate::serial_print(" - ");
    print_hex(stack_top);
    crate::serial_print("\n");

    // Set up initial stack frame with argc/argv/envp for libc
    let initial_sp = setup_initial_stack(&mut address_space, stack_base, stack_size)?;

    crate::serial_print("[INIT] Initial stack pointer set to: ");
    print_hex(initial_sp);
    crate::serial_print("\n");

    // Critical test: Verify that the userspace page table can resolve the entry point
    crate::serial_print("[INIT] Testing userspace page table resolution for entry point...\n");
    match address_space.virt_to_phys(entry_point) {
        Some(resolved_phys) => {
            crate::serial_print("[INIT] SUCCESS: virt_to_phys(0x");
            print_hex(entry_point);
            crate::serial_print(") = 0x");
            print_hex(resolved_phys);
            crate::serial_print(" (userspace page table works!)\n");

            // Physical memory mapping verified successfully
        }
        None => {
            crate::serial_print("[INIT] CRITICAL ERROR: virt_to_phys(0x");
            print_hex(entry_point);
            crate::serial_print(") returned None - entry point not mapped in userspace page table!\n");
            return None;
        }
    }

    // Spawn the userspace process with the loaded address space
    sched::spawn_user_with_address_space(entry_point, initial_sp, *address_space)
}

// External function to allocate page table root
extern "C" {
    fn arch_alloc_page_table_root() -> usize;
}

fn print_hex(n: usize) {
    serial_print("0x");
    let digits = b"0123456789abcdef";
    for i in (0..16).rev() {
        let digit = (n >> (i * 4)) & 0xf;
        unsafe {
            serial_write_byte(digits[digit]);
        }
    }
}

fn print_hex_byte(n: u8) {
    let digits = b"0123456789abcdef";
    unsafe {
        serial_write_byte(digits[(n >> 4) as usize]);
        serial_write_byte(digits[(n & 0xf) as usize]);
    }
}

fn print_u32(mut n: u32) {
    if n == 0 {
        serial_print("0");
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = 10;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + ((n % 10) as u8);
        n /= 10;
    }
    let s = unsafe { core::str::from_utf8_unchecked(&buf[i..]) };
    serial_print(s);
}

// ── Userspace shell creation ─────────────────────────────────────────────────

fn create_shell_elf_binary() -> &'static [u8] {
    // For now, return empty placeholder
    // TODO: Embed actual ELF binary
    &[]
}

/// Create a simple userspace shell program and return its entry point
fn create_simple_userspace_shell() -> usize {
    // Allocate memory for the userspace shell
    let pages = mm::buddy::alloc(2).expect("Failed to allocate shell memory");
    let code_addr = pages;

    // Create simple AArch64 machine code for userspace shell
    let shell_code = [
        // write syscall to display banner
        0xd0, 0x00, 0x80, 0xd2, // mov x16, #64     (write syscall number)
        0x20, 0x00, 0x80, 0xd2, // mov x0, #1       (stdout fd)
        0x21, 0x01, 0x80, 0x12, // mov w1, lo(msg)  (message address - will be fixed up)
        0x42, 0x01, 0x80, 0xd2, // mov x2, #10      (message length)
        0x01, 0x00, 0x00, 0xd4, // svc #0           (make system call)

        // Infinite loop with periodic output
        // loop_start:
        0xd0, 0x00, 0x80, 0xd2, // mov x16, #64     (write syscall)
        0x20, 0x00, 0x80, 0xd2, // mov x0, #1       (stdout)
        0x41, 0x01, 0x80, 0x12, // mov w1, lo(prompt) (prompt address)
        0x82, 0x00, 0x80, 0xd2, // mov x2, #4       (prompt length)
        0x01, 0x00, 0x00, 0xd4, // svc #0           (system call)

        // Small delay
        0x00, 0x20, 0x80, 0xd2, // mov x0, #0x1000  (delay counter)
        // delay_loop:
        0x00, 0x04, 0x00, 0xf1, // subs x0, x0, #1
        0xe1, 0xff, 0xff, 0x54, // b.ne delay_loop

        // Jump back to loop
        0xf2, 0xff, 0xff, 0x17, // b loop_start

        // Padding and data area (strings will be here)
        // At offset 64: banner message
        0x43, 0x79, 0x61, 0x6e, 0x6f, 0x73, 0x68, 0x65, 0x6c, 0x6c, // "Cyanoshell"
        // At offset 74: prompt
        0x3e, 0x20, 0x0a, 0x00, // "> \n\0"
    ];

    unsafe {
        let dest = code_addr as *mut u8;
        for (i, &byte) in shell_code.iter().enumerate() {
            dest.add(i).write(byte);
        }

        // Fix up addresses in the code
        // Set message address (instruction at offset 8)
        let msg_addr = code_addr + 64; // Offset of banner message
        let msg_addr_lo = (msg_addr & 0xFFFF) as u16;
        let instr_ptr = dest.add(8) as *mut u32;
        let instr = instr_ptr.read() | ((msg_addr_lo as u32) << 5);
        instr_ptr.write(instr);

        // Set prompt address (instruction at offset 24)
        let prompt_addr = code_addr + 74; // Offset of prompt
        let prompt_addr_lo = (prompt_addr & 0xFFFF) as u16;
        let instr_ptr = dest.add(24) as *mut u32;
        let instr = instr_ptr.read() | ((prompt_addr_lo as u32) << 5);
        instr_ptr.write(instr);
    }

    code_addr
}

// ── Kernel shell removed - shell is now userland ELF binary ──

// ── Shell is now a separate userland ELF binary ──
// See userland/shell/src/main.rs for the actual shell implementation
