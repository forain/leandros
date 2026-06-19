//! Leandros kernel entry point.

#![no_std]
#![no_main]

extern crate alloc;

mod init;
mod syscall;
mod mem;

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(include_str!("entry_aarch64.s"));
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(include_str!("entry_x86_64.s"));

#[repr(C, align(4096))]
pub struct PageAligned<const N: usize>([u8; N]);

#[no_mangle]
pub static mut EARLY_STACK: PageAligned<0x10000> = PageAligned([0u8; 0x10000]);

#[no_mangle]
pub static mut early_pgtables: PageAligned<32768> = PageAligned([0u8; 32768]);

#[global_allocator]
static ALLOCATOR: mm::slab::SlabAllocator = mm::slab::SlabAllocator;

// ── Limine Revision 6 Requests ───────────────────────────────────────────────

#[used]
#[link_section = ".limine_reqs"]
static BASE_REVISION: limine::BaseRevision = limine::BaseRevision::with_revision(6);

#[used]
#[link_section = ".limine_reqs_start"]
static START_MARKER: limine::RequestsStartMarker = limine::RequestsStartMarker::new();

#[used]
#[link_section = ".limine_reqs_end"]
static END_MARKER: limine::RequestsEndMarker = limine::RequestsEndMarker::new();

#[used]
#[link_section = ".limine_reqs"]
static HHDM_REQUEST: limine::request::HhdmRequest = limine::request::HhdmRequest::new();

#[used]
#[link_section = ".limine_reqs"]
static MEMMAP_REQUEST: limine::request::MemmapRequest = limine::request::MemmapRequest::new();

#[used]
#[link_section = ".limine_reqs"]
static FRAMEBUFFER_REQUEST: limine::request::FramebufferRequest = limine::request::FramebufferRequest::new();

#[used]
#[link_section = ".limine_reqs"]
static MODULE_REQUEST: limine::request::ModulesRequest = limine::request::ModulesRequest::new();

#[used]
#[link_section = ".limine_reqs"]
static RSDP_REQUEST: limine::request::RsdpRequest = limine::request::RsdpRequest::new();

#[used]
#[link_section = ".limine_reqs"]
static KERNEL_ADDR_REQUEST: limine::request::ExecutableAddressRequest = limine::request::ExecutableAddressRequest::new();

#[used]
#[link_section = ".limine_reqs"]
static DTB_REQUEST: limine::request::DtbRequest = limine::request::DtbRequest::new();

use core::sync::atomic::{AtomicUsize, Ordering};

pub static BOOT_INFO_PTR: AtomicUsize = AtomicUsize::new(0);
static mut BOOT_INFO: boot::BootInfo = boot::BootInfo {
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


extern "C" {
    pub fn arch_flush_cache_range(addr: usize, len: usize);
}

#[no_mangle]
pub static KERNEL_CONSOLE_ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

#[no_mangle]
pub extern "C" fn serial_write_byte(b: u8) {
    // Always write to serial, it's fast and safe
    #[cfg(target_arch = "x86_64")]
    unsafe { arch_x86_64::putc(b); }
    #[cfg(target_arch = "aarch64")]
    unsafe { arch_aarch64::uart::putc(b); }

    if !KERNEL_CONSOLE_ENABLED.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }

    // Use a per-CPU re-entrancy guard to avoid deadlocks/character loss
    static IN_WRITE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    
    if !IN_WRITE.swap(true, core::sync::atomic::Ordering::SeqCst) {
        drivers::framebuffer::fb_putc(b);
        drivers::framebuffer::fb_flush();
        IN_WRITE.store(false, core::sync::atomic::Ordering::SeqCst);
    }
}

/// Direct serial write bypassing the framebuffer to avoid recursion.
#[no_mangle]
pub unsafe extern "C" fn serial_write_byte_direct(b: u8) {
    #[cfg(target_arch = "x86_64")]
    arch_x86_64::putc(b);
    #[cfg(target_arch = "aarch64")]
    arch_aarch64::uart::putc(b);
}

#[no_mangle]
pub unsafe extern "C" fn arch_serial_putc(c: u8) { 
    serial_write_byte_direct(c); 
}

#[no_mangle]
pub extern "C" fn print_number(n: u32) {
    if n == 0 { serial_write_byte(b'0'); return; }
    let mut buf = [0u8; 10];
    let mut i = 0;
    let mut num = n;
    while num > 0 { buf[i] = b'0' + (num % 10) as u8; num /= 10; i += 1; }
    for j in (0..i).rev() { serial_write_byte(buf[j]); }
}

#[no_mangle]
pub extern "C" fn print_hex(n: usize) {
    let digits = b"0123456789ABCDEF";
    for i in (0..16).rev() { serial_write_byte(digits[(n >> (i * 4)) & 0xF]); }
}

pub fn serial_print_hex(n: usize) {
    serial_write_byte(b'0');
    serial_write_byte(b'x');
    print_hex(n);
}

#[no_mangle]
pub extern "C" fn kernel_set_console_enabled(enabled: bool) {
    KERNEL_CONSOLE_ENABLED.store(enabled, core::sync::atomic::Ordering::SeqCst);
    serial_print_str("[KERN] Console enabled = ");
    crate::print_number(if enabled { 1 } else { 0 });
    serial_print_str("\n");
}
#[no_mangle]
pub extern "C" fn serial_print(s: *const u8, len: usize) {
    if s.is_null() { return; }
    if len > 65536 {
        // Log crazy length as a direct write to avoid recursion
        let msg = b"\n[KERN] ERROR: serial_print called with crazy length!\n";
        for &b in msg { unsafe { serial_write_byte_direct(b); } }
        return;
    }
    let bytes = unsafe { core::slice::from_raw_parts(s, len) };
    for &b in bytes {
        serial_write_byte(b);
    }
}

#[no_mangle]
pub extern "C" fn serial_print_str_raw(s: *const u8, len: usize) {
    serial_print(s, len);
}

pub fn serial_print_str(msg: &str) {
    for &b in msg.as_bytes() { serial_write_byte(b); }
}

pub fn serial_write_raw(bytes: &[u8]) {
    for &b in bytes { serial_write_byte(b); }
}

pub fn serial_has_data() -> bool {
    unsafe {
        #[cfg(target_arch = "x86_64")]
        return arch_x86_64::serial_has_data();
        #[cfg(target_arch = "aarch64")]
        return arch_aarch64::uart::has_data();
    }
}

pub fn serial_read_byte() -> Option<u8> {
    unsafe {
        #[cfg(target_arch = "x86_64")]
        return arch_x86_64::serial_read_byte();
        #[cfg(target_arch = "aarch64")]
        return arch_aarch64::uart::getc();
    }
}

#[no_mangle]
pub extern "C" fn kernel_main(boot_info_addr: usize) -> ! {
    let is_limine = HHDM_REQUEST.response().is_some();
    let mut hhdm_offset = 0xffff800000000000;
    if is_limine {
        unsafe {
            BOOT_INFO = boot::limine::parse_with_requests(
                &HHDM_REQUEST,
                &MEMMAP_REQUEST,
                &FRAMEBUFFER_REQUEST,
                &MODULE_REQUEST,
                &RSDP_REQUEST,
                &KERNEL_ADDR_REQUEST,
                &DTB_REQUEST,
            );
            hhdm_offset = BOOT_INFO.hhdm_offset;

            #[cfg(target_arch = "aarch64")]
            {
                // If Limine didn't find DTB or hardware, try searching in RAM
                if BOOT_INFO.uart_base == 0 || BOOT_INFO.pci_ecam_base == 0 {
                    serial_print_str("[MAIN] DTB not found, searching in RAM...\n");
                    // Search in first 32MB of RAM (QEMU virt RAM starts at 0x40000000)
                    let start = 0x40000000 + hhdm_offset as usize;
                    let mut found = false;
                    for i in 0..8192 {
                        let addr = start + i * 4096;
                        if boot::device_tree::is_valid_dtb(addr) {
                            serial_print_str("[MAIN] Found DTB in RAM at ");
                            serial_print_hex(addr - hhdm_offset as usize);
                            serial_print_str("\n");
                            let dtb_info = boot::device_tree::parse(addr);
                            if BOOT_INFO.uart_base == 0 { BOOT_INFO.uart_base = dtb_info.uart_base; }
                            if BOOT_INFO.pci_ecam_base == 0 { BOOT_INFO.pci_ecam_base = dtb_info.pci_ecam_base; }
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        serial_print_str("[MAIN] DTB search failed. Using defaults for QEMU virt.\n");
                        if BOOT_INFO.uart_base == 0 { BOOT_INFO.uart_base = 0x09000000; }
                        if BOOT_INFO.pci_ecam_base == 0 { BOOT_INFO.pci_ecam_base = 0x3F000000; }
                    }
                }
            }
        }
    } else {
        #[cfg(target_arch = "aarch64")]
        {
            let mut dtb_addr = boot_info_addr;
            // First check if boot_info_addr looks valid
            if dtb_addr == 0 || !unsafe { boot::device_tree::is_valid_dtb(dtb_addr) } {
                 dtb_addr = 0;
            }
            
            let boot_info = if dtb_addr != 0 {
                unsafe { boot::device_tree::parse(dtb_addr) }
            } else {
                // Fallback for QEMU virt machine: 1GB RAM at 0x40000000
                static mut FALLBACK_MM: [boot::MemoryRegion; 1] = [boot::MemoryRegion {
                    base: 0x40000000,
                    length: 0x40000000,
                    kind: boot::MemoryType::Available,
                }];
                boot::BootInfo {
                    memory_map: core::ptr::addr_of!(FALLBACK_MM) as *const boot::MemoryRegion,
                    memory_map_len: 1,
                    framebuffer_base: 0,
                    framebuffer_width: 0,
                    framebuffer_height: 0,
                    framebuffer_pitch: 0,
                    rsdp_addr:           0,
                    uart_base:           0,
                    pci_ecam_base:       0,
                    initrd_base:         0,
                    initrd_size:         0,
                    hhdm_offset:         0,
                }
            };
            unsafe {
                BOOT_INFO = boot_info;
                BOOT_INFO.hhdm_offset = hhdm_offset;
            }
        }
        #[cfg(target_arch = "x86_64")]
        {
            unsafe { 
                BOOT_INFO = boot::multiboot2::parse(boot_info_addr);
                BOOT_INFO.hhdm_offset = hhdm_offset;
            }
        }
    }

    BOOT_INFO_PTR.store(&raw mut BOOT_INFO as usize, Ordering::SeqCst);

    mm::init_with_map(unsafe { (*core::ptr::addr_of!(BOOT_INFO)).memory_regions() }, hhdm_offset as usize);

    #[cfg(target_arch = "x86_64")] { arch_x86_64::init(unsafe { &*core::ptr::addr_of!(BOOT_INFO) }); }
    #[cfg(target_arch = "aarch64")] { arch_aarch64::init(unsafe { &*core::ptr::addr_of!(BOOT_INFO) }); }

    // NOW we can print safely
    serial_print_str("[MAIN] Architecture initialized.\n");

    if is_limine {
        serial_print_str("[MAIN] Limine boot info parsed. Memmap len: ");
        serial_print_hex(unsafe { BOOT_INFO.memory_map_len });
        serial_print_str(" HHDM: ");
        serial_print_hex(hhdm_offset as usize);
        serial_print_str("\n");
        serial_print_str("[MAIN] UART base: ");
        serial_print_hex(unsafe { BOOT_INFO.uart_base as usize });
        serial_print_str(" PCI ECAM: ");
        serial_print_hex(unsafe { BOOT_INFO.pci_ecam_base as usize });
        serial_print_str("\n");
    }

    // Initialize PCI
    if unsafe { (*core::ptr::addr_of!(BOOT_INFO)).pci_ecam_base } != 0 {
        let pci_phys = unsafe { (*core::ptr::addr_of!(BOOT_INFO)).pci_ecam_base as usize };
        let pci_virt = pci_phys + hhdm_offset as usize;
        serial_print_str("[MAIN] Initializing PCI ECAM at ");
        serial_print_hex(pci_phys);
        serial_print_str("\n");
        drivers::pci::init_pci(pci_virt);
    }

    // NOW we can print
    serial_print_str("[MAIN] Architecture initialized.\n");

    if is_limine {
        serial_print_str("[MAIN] Limine boot info parsed. Memmap len: ");
        serial_print_hex(unsafe { BOOT_INFO.memory_map_len });
        serial_print_str(" HHDM: ");
        serial_print_hex(hhdm_offset as usize);
        serial_print_str("\n");
        serial_print_str("[MAIN] UART base: ");
        serial_print_hex(unsafe { BOOT_INFO.uart_base as usize });
        serial_print_str(" PCI ECAM: ");
        serial_print_hex(unsafe { BOOT_INFO.pci_ecam_base as usize });
        serial_print_str("\n");
    }

    // Debug HHDM setup
    serial_print_str("[MM] Initializing memory management with HHDM offset: ");
    serial_print_hex(hhdm_offset as usize);
    serial_print_str("\n");

    unsafe {
        let bi = &*core::ptr::addr_of!(BOOT_INFO);
        if bi.framebuffer_base != 0 {
            serial_print_str("[MAIN] Initializing framebuffer console: ");
            serial_print_hex(bi.framebuffer_base as usize);
            serial_print_str(" ");
            print_number(bi.framebuffer_width);
            serial_print_str("x");
            print_number(bi.framebuffer_height);
            serial_print_str(" pitch=");
            print_number(bi.framebuffer_pitch);
            serial_print_str("\n");

            // Set VFS framebuffer info for DRM driver
            let width = bi.framebuffer_width;
            let height = bi.framebuffer_height;
            let pitch = bi.framebuffer_pitch;

            // Ensure pitch is in bytes
            let pitch_bytes = if pitch < width * 4 { width * 4 } else { pitch };

            vfs_server::set_framebuffer(bi.framebuffer_base, width, height, pitch_bytes);
            
            // Also set it in the framebuffer driver for KMS integration
            drivers::framebuffer::set_boot_framebuffer(bi.framebuffer_base, width, height, pitch_bytes);

            // Verify it was set
            if drivers::framebuffer::get_hardware_fb_info().is_some() {
                serial_print_str("[MAIN] BOOT_FB registered successfully\n");
            } else {
                serial_print_str("[MAIN] BOOT_FB registration FAILED\n");
            }
            let fb_virt = mm::phys_to_virt(bi.framebuffer_base as usize);
            serial_print_str("[MAIN] Framebuffer virtual address: ");
            serial_print_hex(fb_virt);
            serial_print_str("\n");

            drivers::framebuffer::init_kernel_fb(
                fb_virt as *mut u32,
                width as usize,
                height as usize,
                pitch_bytes as usize,
            );
            serial_print_str("[MAIN] Framebuffer console initialized.\n");
        } else {
            serial_print_str("[MAIN] No framebuffer base in boot info.\n");
        }
        
        serial_print_str("\n[LEANDROS] Kernel starting...\n");
        serial_print_str("[TRACE] boot_info_addr: ");
        serial_print_hex(boot_info_addr);
        serial_print_str("\n");

        init::init_task_main(bi);
    }
    
    loop { core::hint::spin_loop(); }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    serial_print_str("\n--- KERNEL PANIC ---\n");
    if let Some(msg) = info.message().as_str() {
        serial_print_str(msg);
    }
    if let Some(loc) = info.location() {
        serial_print_str("\nLocation: ");
        serial_print_str(loc.file());
        serial_print_str(":");
        print_number(loc.line());
    }
    serial_print_str("\n--------------------\n");
    loop { core::hint::spin_loop(); }
}
