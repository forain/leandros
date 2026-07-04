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

/// Set to 1 by the aarch64 entry stub when the kernel was entered at EL2.
/// Read by arch-aarch64's PSCI code to pick the HVC vs SMC conduit.
/// Placed in .data explicitly: it is written before the BSS-zero loop runs.
#[cfg(target_arch = "aarch64")]
#[no_mangle]
#[used]
#[link_section = ".data"]
pub static mut boot_entered_el2: u64 = 0;

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
        // fb_flush only transfers the dirty region (typically a single character
        // cell), so flushing per character is cheap and keeps the shell prompt
        // and typed input visible.  On x86 the framebuffer is a host-visible
        // linear surface and fb_flush is a no-op.
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

            // aarch64 DTB fallback: deferred to after arch_aarch64::init() because
            // device_tree::parse emits SIMD instructions (struct zeroing), and SIMD
            // is not enabled until enable_identity() runs inside arch::init().
        }
    } else {
        #[cfg(target_arch = "aarch64")]
        {
            let mut dtb_addr = boot_info_addr;
            // First check if boot_info_addr (x0) looks valid. QEMU `-kernel`
            // with an ELF image enters with x0 = 0 rather than the DTB pointer,
            // so this usually fails and we fall through to the RAM scan below.
            if dtb_addr == 0 || !unsafe { boot::device_tree::is_valid_dtb(dtb_addr) } {
                dtb_addr = 0;
            }

            // RAM scan: QEMU still synthesises a DTB (with /chosen initrd-start
            // /-end and the pl011/pcie reg windows) and loads it into guest RAM
            // even when x0 isn't set. The early page tables map 0..4GB through
            // the HHDM, so scan that window for the FDT magic (0xD00DFEED).
            // The DTB is page-aligned, so step by 4 KiB.
            if dtb_addr == 0 {
                let scan_start = 0x4000_0000usize + hhdm_offset as usize;
                let scan_end   = 0x8000_0000usize + hhdm_offset as usize;
                let mut a = scan_start;
                while a < scan_end {
                    if unsafe { boot::device_tree::is_valid_dtb(a) } {
                        dtb_addr = a;
                        break;
                    }
                    a += 0x1000;
                }
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

                // QEMU `-kernel` provides neither a DTB nor ACPI for a bare ELF,
                // so the device-tree scan above finds nothing and UART/ECAM stay
                // zero. Fall back to the fixed QEMU virt MMIO addresses: the PL011
                // UART is always at 0x0900_0000, and with `-cpu max` (48-bit PA)
                // the PCIe ECAM is the high window at 0x40_1000_0000 — the same
                // base the Limine path discovers via ACPI MCFG. Without this the
                // PCI bus scan finds no devices (e.g. virtio-sound).
                if BOOT_INFO.uart_base == 0 { BOOT_INFO.uart_base = 0x0900_0000; }
                if BOOT_INFO.pci_ecam_base == 0 { BOOT_INFO.pci_ecam_base = 0x40_1000_0000; }
            }

            // Direct boot: the DTB memory map calls *all* RAM available, unlike
            // Limine which marks the kernel image and modules reserved. Reserve
            // them here so the buddy allocator never hands out frames that alias
            // the live kernel image (its page tables live in .bss) or the initrd
            // — doing so corrupts page tables and faults with translation errors.
            //
            // Direct aarch64 links the kernel at KERNEL_VIRT = 0xffff_8000_0000_0000
            // + KERNEL_PHYS = 0x4008_0000 (see linkers/aarch64-direct.ld).
            const KERNEL_VIRT: usize = 0xffff_8000_0000_0000;
            const KERNEL_PHYS: usize = 0x4008_0000;
            extern "C" { static __bss_end: u8; }
            let kernel_end_phys =
                core::ptr::addr_of!(__bss_end) as usize - KERNEL_VIRT;
            mm::buddy::reserve_range(KERNEL_PHYS, kernel_end_phys);

            // The initrd is loaded at a fixed physical address by run-qemu.sh's
            // `-device loader` (see scripts/run-qemu.sh). The early page tables
            // still identity-map low RAM, so we can read it here (before the
            // HHDM/buddy come up) to learn its real size, reserve exactly that,
            // and record it in BootInfo for init/execve to use later.
            const INITRD_PHYS: usize = 0x4800_0000;
            let initrd_len = init::cpio_image_size(INITRD_PHYS);
            if initrd_len > 0 {
                mm::buddy::reserve_range(INITRD_PHYS, INITRD_PHYS + initrd_len);
                unsafe {
                    BOOT_INFO.initrd_base = INITRD_PHYS as u64;
                    BOOT_INFO.initrd_size = initrd_len as u64;
                }
            }
        }
        #[cfg(target_arch = "x86_64")]
        {
            unsafe {
                BOOT_INFO = boot::multiboot2::parse(boot_info_addr);

                // Direct boot via SeaBIOS multiboot1 (or PVH) provides no
                // multiboot2 MBI, so parse() finds no memory map. Fall back to a
                // fixed QEMU map. The PVH trampoline maps the low 2 GiB into the
                // HHDM, so usable RAM must stay within that window.
                if BOOT_INFO.memory_map_len == 0 {
                    static mut FALLBACK_MM: [boot::MemoryRegion; 1] = [boot::MemoryRegion {
                        base:   0x0010_0000,
                        length: 0x7F00_0000 - 0x0010_0000,
                        kind:   boot::MemoryType::Available,
                    }];
                    BOOT_INFO.memory_map = core::ptr::addr_of!(FALLBACK_MM) as *const boot::MemoryRegion;
                    BOOT_INFO.memory_map_len = 1;

                    // Reserve the kernel image (loaded at phys 0x10_0000; the
                    // body is linked in the higher half, so subtract
                    // KERNEL_OFFSET to recover the physical end) and the
                    // fixed-address initrd, so the buddy allocator never hands
                    // out frames aliasing them.
                    const KERNEL_OFFSET: usize = 0xffff_ffff_8000_0000;
                    extern "C" { static __bss_end: u8; }
                    let kernel_end_phys =
                        (core::ptr::addr_of!(__bss_end) as usize) - KERNEL_OFFSET;
                    mm::buddy::reserve_range(0x0010_0000, kernel_end_phys);

                    // initrd is placed here by run-qemu.sh's -device loader. The
                    // trampoline identity-maps the low 2 GiB, so it is readable
                    // now (before the HHDM/buddy come up) to size and reserve it.
                    const INITRD_PHYS: usize = 0x1000_0000;
                    let initrd_len = init::cpio_image_size(INITRD_PHYS);
                    if initrd_len > 0 {
                        mm::buddy::reserve_range(INITRD_PHYS, INITRD_PHYS + initrd_len);
                        BOOT_INFO.initrd_base = INITRD_PHYS as u64;
                        BOOT_INFO.initrd_size = initrd_len as u64;
                    }
                }

                BOOT_INFO.hhdm_offset = hhdm_offset;
            }
        }
    }

    BOOT_INFO_PTR.store(&raw mut BOOT_INFO as usize, Ordering::SeqCst);

    mm::init_with_map(unsafe { (*core::ptr::addr_of!(BOOT_INFO)).memory_regions() }, hhdm_offset as usize);

    #[cfg(target_arch = "x86_64")] { arch_x86_64::init(unsafe { &*core::ptr::addr_of!(BOOT_INFO) }); }
    #[cfg(target_arch = "aarch64")] { arch_aarch64::init(unsafe { &*core::ptr::addr_of!(BOOT_INFO) }); }

    // aarch64: SIMD and UART are now available. Run DTB fallback search if Limine
    // didn't provide uart_base/pci_ecam_base via its DTB request.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        if is_limine && (BOOT_INFO.uart_base == 0 || BOOT_INFO.pci_ecam_base == 0) {
            serial_print_str("[MAIN] DTB not found via Limine, searching in RAM...\n");
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
                serial_print_str("[MAIN] DTB search failed.\n");
                // UART is always at 0x09000000 on QEMU virt regardless of highmem setting.
                if BOOT_INFO.uart_base == 0 { BOOT_INFO.uart_base = 0x09000000; }
                // Try ACPI MCFG for ECAM — QEMU UEFI boot provides ACPI, not DTB.
                // The ECAM base moved to 0x4010000000 in QEMU 5.0+ (highmem=on default).
                if BOOT_INFO.pci_ecam_base == 0 && BOOT_INFO.rsdp_addr != 0 {
                    let ecam = boot::acpi::find_ecam_base(BOOT_INFO.rsdp_addr, hhdm_offset);
                    if ecam != 0 {
                        serial_print_str("[MAIN] ECAM found via ACPI MCFG at 0x");
                        serial_print_hex(ecam as usize);
                        serial_print_str("\n");
                        BOOT_INFO.pci_ecam_base = ecam;
                    } else {
                        serial_print_str("[MAIN] ACPI MCFG ECAM not found; skipping PCI.\n");
                    }
                }
            }
        }
    }

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
        // On aarch64 the ECAM is not covered by Limine's HHDM (HHDM only maps RAM).
        // Map the bus-0 ECAM window (1 bus × 32 devs × 8 fns × 4 KiB = 1 MiB) with
        // device-nGnRE attributes so PCI config-space reads don't fault.
        // We round up to 2 MiB so the mapping fills exactly one L2 page table entry.
        #[cfg(target_arch = "aarch64")]
        unsafe { arch_aarch64::map_mmio_range(pci_phys, 0x0020_0000, hhdm_offset as usize); }
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
            // No bootloader-provided framebuffer.  This is the normal case on
            // AArch64 with virtio-gpu-pci: unlike x86 virtio-vga, it exposes no
            // VGA/GOP linear framebuffer for Limine to report.  Bring up the
            // VirtIO GPU ourselves and create a scanout-backed RAM surface so the
            // kernel console has somewhere to draw.
            serial_print_str("[MAIN] No bootloader framebuffer; trying VirtIO GPU...\n");
            // Defaults used only if the GPU does not report a preferred mode;
            // setup_console_framebuffer returns the dimensions actually programmed.
            const DEFAULT_WIDTH: u32 = 1024;
            const DEFAULT_HEIGHT: u32 = 768;
            if let Some((fb_phys, fb_virt, width, height, pitch_bytes)) =
                drivers::virtio_gpu::setup_console_framebuffer(DEFAULT_WIDTH, DEFAULT_HEIGHT)
            {
                serial_print_str("[MAIN] VirtIO GPU framebuffer at phys=");
                serial_print_hex(fb_phys as usize);
                serial_print_str(" virt=");
                serial_print_hex(fb_virt);
                serial_print_str(" ");
                print_number(width);
                serial_print_str("x");
                print_number(height);
                serial_print_str("\n");

                vfs_server::set_framebuffer(fb_phys, width, height, pitch_bytes);
                drivers::framebuffer::set_boot_framebuffer(fb_phys, width, height, pitch_bytes);
                drivers::framebuffer::init_kernel_fb(
                    fb_virt as *mut u32,
                    width as usize,
                    height as usize,
                    pitch_bytes as usize,
                );
                serial_print_str("[MAIN] VirtIO GPU framebuffer console initialized.\n");
            } else {
                serial_print_str("[MAIN] No VirtIO GPU framebuffer available.\n");
            }
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
