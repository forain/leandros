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
#[link_section = ".data"]
pub static mut early_pgtables: PageAligned<32768> = PageAligned([0u8; 32768]);

#[global_allocator]
static ALLOCATOR: mm::slab::SlabAllocator = mm::slab::SlabAllocator;

// ── Limine Revision 6 Requests ───────────────────────────────────────────────

#[used]
#[link_section = ".limine_reqs_start"]
static START_MARKER: limine::RequestsStartMarker = limine::RequestsStartMarker::new();

#[used]
#[link_section = ".limine_reqs"]
static BASE_REVISION: limine::BaseRevision = limine::BaseRevision::new();

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

#[used]
#[link_section = ".limine_reqs_end"]
static END_MARKER: limine::RequestsEndMarker = limine::RequestsEndMarker::new();

use core::sync::atomic::{AtomicUsize, Ordering};

pub static BOOT_INFO_PTR: AtomicUsize = AtomicUsize::new(0);
static mut BOOT_INFO: boot::BootInfo = boot::BootInfo {
    memory_map: core::ptr::null(),
    memory_map_len: 0,
    framebuffer_base: 0,
    framebuffer_width: 0,
    framebuffer_height: 0,
    framebuffer_pitch: 0,
    rsdp_addr: 0,
    uart_base: 0,
    initrd_base: 0,
    initrd_size: 0,
    hhdm_offset: 0,
};

extern "C" {
    pub fn arch_flush_cache_range(addr: usize, len: usize);
}

#[no_mangle]
pub extern "C" fn serial_write_byte(b: u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe { arch_x86_64::putc(b); }
    #[cfg(target_arch = "aarch64")]
    unsafe { arch_aarch64::uart::putc(b); }

    drivers::framebuffer::fb_putc(b);
}

#[no_mangle]
pub unsafe extern "C" fn serial_print(ptr: *const u8, len: usize) {
    let slice = core::slice::from_raw_parts(ptr, len);
    for &b in slice { serial_write_byte(b); }
}

#[no_mangle]
pub unsafe extern "C" fn serial_print_bytes(ptr: *const u8, len: usize) {
    let slice = core::slice::from_raw_parts(ptr, len);
    for &b in slice { serial_write_byte(b); }
}

#[no_mangle]
pub unsafe extern "C" fn arch_serial_putc(c: u8) { serial_write_byte(c); }

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
    serial_write_byte(b'0');
    serial_write_byte(b'x');
    for i in (0..16).rev() { serial_write_byte(digits[(n >> (i * 4)) & 0xF]); }
}

#[no_mangle]
pub extern "C" fn serial_print_str_raw(ptr: *const u8, len: usize) {
    let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
    for &b in slice { serial_write_byte(b); }
}

pub fn serial_print_str(msg: &str) {
    serial_print_str_raw(msg.as_ptr(), msg.len());
}

#[no_mangle]
pub fn serial_write_raw(msg: &[u8]) {
    for &b in msg { serial_write_byte(b); }
}

#[no_mangle]
pub fn serial_read_byte() -> Option<u8> {
    #[cfg(target_arch = "x86_64")]
    unsafe { arch_x86_64::serial_read_byte() }
    #[cfg(target_arch = "aarch64")]
    unsafe { arch_aarch64::uart::getc() }
}

#[no_mangle]
pub fn serial_has_data() -> bool {
    #[cfg(target_arch = "x86_64")]
    unsafe { arch_x86_64::serial_has_data() }
    #[cfg(target_arch = "aarch64")]
    unsafe { arch_aarch64::uart::has_data() }
}

#[no_mangle]
pub extern "C" fn kernel_main(boot_info_addr: usize) -> ! {
    let is_limine = HHDM_REQUEST.response().is_some();
    let mut hhdm_offset = 0xffff800000000000;
    if is_limine {
        hhdm_offset = HHDM_REQUEST.response().unwrap().offset;
    }

    if !is_limine {
        #[cfg(target_arch = "aarch64")]
        {
            let mut dtb_addr = boot_info_addr;
            if dtb_addr == 0 || !unsafe { boot::device_tree::is_valid_dtb(dtb_addr) } {
                serial_print_str("[MAIN] DTB invalid, scanning RAM...\n");
                for i in 0..(256 * 1024 * 1024 / 4) {
                    let addr = 0x40000000 + i * 4;
                    unsafe {
                        let ptr = addr as *const u32;
                        let val = core::ptr::read_volatile(ptr);
                        if val == 0xD00DFEED || val == 0xEDFE0DD0 {
                             if boot::device_tree::is_valid_dtb(addr) {
                                dtb_addr = addr;
                                serial_print_str("[MAIN] Found DTB at ");
                                print_hex(dtb_addr);
                                serial_print_str("\n");
                                break;
                             }
                        }
                    }
                }
            }
            let boot_info = if dtb_addr != 0 {
                unsafe { boot::device_tree::parse(dtb_addr) }
            } else {
                boot::BootInfo {
                    memory_map: core::ptr::null(),
                    memory_map_len: 0,
                    framebuffer_base: 0,
                    framebuffer_width: 0,
                    framebuffer_height: 0,
                    framebuffer_pitch: 0,
                    rsdp_addr: 0,
                    uart_base: 0,
                    initrd_base: 0,
                    initrd_size: 0,
                    hhdm_offset: 0,
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

    if is_limine {
        unsafe {
            BOOT_INFO = boot::limine::parse_with_requests(
                &HHDM_REQUEST, &MEMMAP_REQUEST, &FRAMEBUFFER_REQUEST, &MODULE_REQUEST,
                &RSDP_REQUEST, &KERNEL_ADDR_REQUEST, &DTB_REQUEST,
            );
            BOOT_INFO.hhdm_offset = hhdm_offset;
        }
    }

    unsafe {
        BOOT_INFO_PTR.store(&raw mut BOOT_INFO as usize, Ordering::SeqCst);
    }

    mm::init_with_map(unsafe { (*core::ptr::addr_of!(BOOT_INFO)).memory_regions() }, hhdm_offset as usize);

    #[cfg(target_arch = "x86_64")] { arch_x86_64::init(unsafe { &*core::ptr::addr_of!(BOOT_INFO) }); }
    #[cfg(target_arch = "aarch64")] { arch_aarch64::init(unsafe { &*core::ptr::addr_of!(BOOT_INFO) }); }

    unsafe {
        if (*core::ptr::addr_of!(BOOT_INFO)).framebuffer_base != 0 {
            drivers::framebuffer::init_kernel_fb(
                mm::phys_to_virt((*core::ptr::addr_of!(BOOT_INFO)).framebuffer_base as usize) as *mut u32,
                (*core::ptr::addr_of!(BOOT_INFO)).framebuffer_width as usize,
                (*core::ptr::addr_of!(BOOT_INFO)).framebuffer_height as usize,
                (*core::ptr::addr_of!(BOOT_INFO)).framebuffer_pitch as usize,
            );
            drivers::framebuffer::set_boot_framebuffer(
                (*core::ptr::addr_of!(BOOT_INFO)).framebuffer_base,
                (*core::ptr::addr_of!(BOOT_INFO)).framebuffer_width,
                (*core::ptr::addr_of!(BOOT_INFO)).framebuffer_height,
                (*core::ptr::addr_of!(BOOT_INFO)).framebuffer_pitch,
            );
        }
    }

    serial_print_str("\n[LEANDROS] Kernel starting...\n");
    serial_print_str("[TRACE] boot_info_addr: ");
    print_hex(boot_info_addr);
    serial_print_str("\n");

    serial_print_str("[INIT] Scheduler init...\n");
    sched::init();
    serial_print_str("[INIT] Scheduler init done.\n");
    init::init_task_main(unsafe { &*core::ptr::addr_of!(BOOT_INFO) });
    loop { core::hint::spin_loop(); }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    serial_print_str("\n[LEANDROS] KERNEL PANIC: ");
    let mut writer = SerialWriter;
    let _ = core::fmt::write(&mut writer, core::format_args!("{}", info));
    loop { core::hint::spin_loop(); }
}

struct SerialWriter;
impl core::fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result { serial_print_str(s); Ok(()) }
}
