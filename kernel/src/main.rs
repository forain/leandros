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

#[repr(C, align(16))]
pub struct Stack<const N: usize>([u8; N]);

#[no_mangle]
pub static mut EARLY_STACK: Stack<0x10000> = Stack([0u8; 0x10000]);

pub static mut BOOT_INFO_PTR: usize = 0;

// ── Limine Requests ──────────────────────────────────────────────────────────

#[no_mangle]
#[link_section = ".limine_requests_start"]
#[used]
pub static LIMINE_REQUESTS_START_MARKER: [u64; 4] = [0xf6b8f4b39de7d1ae, 0xfab91a6940fcb9cf, 0x785c6ed015d3e316, 0x181e920a7852b9d9];

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static LIMINE_BASE_REVISION: [u64; 3] = [0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, 6];

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static mut KERNEL_ADDR_REQUEST: boot::limine::Request<boot::limine::KernelAddressResponse> = boot::limine::Request {
    id:       [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, 0x71ba76863bc3007b, 0x87d73f452900c67f],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null()),
};

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static mut HHDM_REQUEST: boot::limine::Request<boot::limine::HhdmResponse> = boot::limine::Request {
    id:       [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, 0x48dcf1cb8ad2b852, 0x63984e959a98244b],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null()),
};

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static mut MEMMAP_REQUEST: boot::limine::Request<boot::limine::MemMapResponse> = boot::limine::Request {
    id:       [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, 0x67cf3d9d378a806f, 0xe304acdfc50c3c62],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null()),
};

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static mut FRAMEBUFFER_REQUEST: boot::limine::Request<boot::limine::FramebufferResponse> = boot::limine::Request {
    id:       [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, 0x9d5827dcd881dd75, 0xa3148604f6fab11b],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null()),
};

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static mut RSDP_REQUEST: boot::limine::Request<boot::limine::RsdpResponse> = boot::limine::Request {
    id:       [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, 0xc5e77b6b397e7b43, 0x27637845accdcf3c],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null()),
};

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static mut MODULE_REQUEST: boot::limine::Request<boot::limine::ModuleResponse> = boot::limine::Request {
    id:       [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, 0x3e7e279702be32af, 0xca1c4f3bd1280cee],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null()),
};

#[no_mangle]
#[link_section = ".limine_requests_end"]
#[used]
pub static LIMINE_REQUESTS_END_MARKER: [u64; 2] = [0xadc0e0531bb10d03, 0x9572709f31764c62];

#[global_allocator]
static ALLOCATOR: mm::slab::SlabAllocator = mm::slab::SlabAllocator;

// ── Serial port ──────────────────────────────────────────────────────────────

pub fn serial_write_byte(b: u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        arch_x86_64::putc(b);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        // Direct write to UART data register (assuming QEMU virt base)
        core::arch::asm!(
            "str {val:w}, [{base}]",
            val = in(reg) b as u32,
            base = in(reg) 0x09000000usize,
            options(nostack, nomem)
        );
    }
}

#[no_mangle]
pub unsafe extern "C" fn arch_serial_putc(c: u8) {
    serial_write_byte(c);
}

#[no_mangle]
pub unsafe extern "C" fn serial_print_bytes(ptr: *const u8, len: usize) {
    let slice = core::slice::from_raw_parts(ptr, len);
    for &b in slice { serial_write_byte(b); }
}

pub fn serial_read_byte() -> Option<u8> {
    #[cfg(target_arch = "x86_64")]
    unsafe { arch_x86_64::serial_read_byte() }
    #[cfg(target_arch = "aarch64")]
    None
}

pub fn serial_has_data() -> bool {
    #[cfg(target_arch = "x86_64")]
    unsafe { arch_x86_64::serial_has_data() }
    #[cfg(target_arch = "aarch64")]
    false
}

pub fn serial_write_raw(msg: &[u8]) {
    for &b in msg { serial_write_byte(b); }
}

pub fn serial_print(msg: &str) {
    serial_write_raw(msg.as_bytes());
}

pub fn print_number(n: u32) {
    if n == 0 { serial_write_byte(b'0'); return; }
    let mut buf = [0u8; 10];
    let mut i = 0;
    let mut num = n;
    while num > 0 { buf[i] = b'0' + (num % 10) as u8; num /= 10; i += 1; }
    for j in (0..i).rev() { serial_write_byte(buf[j]); }
}

pub fn print_hex(n: usize) {
    let digits = b"0123456789ABCDEF";
    serial_print("0x");
    for i in (0..16).rev() { serial_write_byte(digits[(n >> (i * 4)) & 0xF]); }
}

// ── Kernel Entry ─────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn kernel_main(boot_info_addr: usize) -> ! {
    serial_write_byte(b'M');
    serial_write_byte(b'1');

    serial_print("\n[LEANDROS] Kernel starting...\n");

    let is_limine = unsafe {
        let resp_ptr = *HHDM_REQUEST.response.get();
        !resp_ptr.is_null()
    };

    let mut boot_info = if is_limine {
        unsafe { boot::limine::parse() }
    } else {
        #[cfg(target_arch = "x86_64")]
        { unsafe { boot::multiboot2::parse(boot_info_addr) } }
        #[cfg(target_arch = "aarch64")]
        { unsafe { boot::device_tree::parse(boot_info_addr) } }
    };

    if is_limine {
        unsafe {
            let resp_ptr = *MODULE_REQUEST.response.get();
            if !resp_ptr.is_null() {
                let resp = &*resp_ptr;
                if resp.module_count > 0 {
                    let module = &**resp.modules;
                    boot_info.initrd_base = (module.address as u64).saturating_sub(boot_info.hhdm_offset);
                    boot_info.initrd_size = module.size;
                }
            }
        }

        let resp_ptr = unsafe { *HHDM_REQUEST.response.get() };
        if !resp_ptr.is_null() {
            let resp = unsafe { &*resp_ptr };
            boot_info.hhdm_offset = resp.offset;
        }
    }

    mm::init_with_map(boot_info.memory_regions(), boot_info.hhdm_offset as usize);
    serial_print("  mm::phys_to_virt(0) = ");
    print_hex(mm::phys_to_virt(0));
    serial_print("\n");

    serial_print("[INIT] Architecture-specific init...\n");
    #[cfg(target_arch = "x86_64")]
    { arch_x86_64::init(&boot_info); }
    #[cfg(target_arch = "aarch64")]
    { arch_aarch64::init(&boot_info); }

    serial_print("[INIT] Scheduler init...\n");
    sched::init();

    unsafe {
        BOOT_INFO_PTR = &boot_info as *const _ as usize;
    }
    init::init_task_main(&boot_info);
}

struct SerialWriter;
impl core::fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        serial_print(s);
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    serial_print("\n[LEANDROS] KERNEL PANIC: ");
    let mut writer = SerialWriter;
    let _ = core::fmt::write(&mut writer, core::format_args!("{}", info));
    loop { core::hint::spin_loop(); }
}
