//! Cyanos kernel entry point.
//!
//! `kernel_main` is called by the arch-specific `_start` stub (entry_*.s)
//! after the stack is set up and BSS is zeroed.  It receives the physical
//! address of the boot information structure (MBI for x86-64, DTB for AArch64).

#![no_std]
#![no_main]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]
#![cfg_attr(target_arch = "x86_64", feature(sync_unsafe_cell))]

extern crate alloc;

use core::panic::PanicInfo;
use core::alloc::{GlobalAlloc, Layout};

// ── Global Allocator ─────────────────────────────────────────────────────────

struct KernelAllocator;
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        mm::slab::alloc(layout.size()).unwrap_or(core::ptr::null_mut())
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        mm::slab::free(ptr, layout.size());
    }
}
#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

// ── Architecture support ─────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(include_str!("entry_x86_64.s"));
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(include_str!("entry_aarch64.s"));

// ── Limine Requests ──────────────────────────────────────────────────────────

mod init;
mod syscall;
mod mem;

/// Global pointer to the boot info structure.
pub static mut BOOT_INFO_PTR: usize = 0;

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static LIMINE_BASE_REVISION: [u64; 3] = [0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, 0];

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static ENTRY_POINT_REQUEST: boot::limine::EntryPointRequest = boot::limine::EntryPointRequest {
    id:       [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, 0x13d86c035a1cd3e1, 0x2b0caa89d8f3026a],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null()),
    entry_point: crate::_start,
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
pub static MEMMAP_REQUEST: boot::limine::Request<boot::limine::MemMapResponse> = boot::limine::Request {
    id:       [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, 0x67cf3d9d378a806f, 0xe304acdfc50c3c62],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null()),
};

#[no_mangle]
#[link_section = ".limine_requests"]
#[used]
pub static FRAMEBUFFER_REQUEST: boot::limine::Request<boot::limine::FramebufferResponse> = boot::limine::Request {
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

// ── Serial port ──────────────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
unsafe fn early_serial_init() {
    use core::arch::asm;
    macro_rules! outb {
        ($port:expr, $val:expr) => {
            asm!("out dx, al", in("dx") $port as u16, in("al") $val as u8,
                 options(nomem, nostack));
        }
    }
    outb!(0x3F9, 0x00u8);
    outb!(0x3FB, 0x80u8);
    outb!(0x3F8, 0x01u8);
    outb!(0x3F9, 0x00u8);
    outb!(0x3FB, 0x03u8);
    outb!(0x3FA, 0xC7u8);
}

#[cfg(target_arch = "aarch64")]
unsafe fn early_serial_init() {}

#[no_mangle]
pub fn serial_read_byte() -> Option<u8> {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::asm;
        let mut status: u8;
        asm!("in al, dx", out("al") status, in("dx") 0x3FDu16, options(nomem, nostack));
        if status & 0x01 != 0 {
            let mut b: u8;
            asm!("in al, dx", out("al") b, in("dx") 0x3F8u16, options(nomem, nostack));
            return Some(b);
        }
        return None;
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let base = 0x09000000usize;
        let fr = (base + 0x18) as *const u32;
        if fr.read_volatile() & (1 << 4) != 0 { return None; }
        let dr = base as *const u32;
        Some((dr.read_volatile() & 0xFF) as u8)
    }
}

#[no_mangle]
pub unsafe fn serial_write_byte(b: u8) {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::asm;
        loop {
            let mut status: u8;
            asm!("in al, dx", out("al") status, in("dx") 0x3FDu16, options(nomem, nostack));
            if status & 0x20 != 0 { break; }
        }
        asm!("out dx, al", in("dx") 0x3F8u16, in("al") b, options(nomem, nostack));
    }
    #[cfg(target_arch = "aarch64")]
    {
        let base = 0x09000000usize;
        let fr = (base + 0x18) as *const u32;
        while fr.read_volatile() & (1 << 5) != 0 {}
        let dr = base as *mut u32;
        dr.write_volatile(b as u32);
    }
}

pub fn serial_print(s: &str) {
    for b in s.bytes() {
        unsafe { serial_write_byte(b); }
        if b == b'\n' { unsafe { serial_write_byte(b'\r'); } }
    }
}

#[no_mangle]
pub fn serial_write_raw(buf: &[u8]) {
    for &b in buf { unsafe { serial_write_byte(b); } }
}

#[no_mangle]
pub extern "C" fn serial_print_bytes(ptr: *const u8, len: usize) {
    if ptr.is_null() { return; }
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    serial_write_raw(bytes);
}

fn print_number(n: u32) {
    let mut buf = [0u8; 10];
    if n == 0 {
        serial_print("0");
        return;
    }
    let mut i = 10usize;
    let mut val = n;
    while val > 0 {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    let s = unsafe { core::str::from_utf8_unchecked(&buf[i..]) };
    serial_print(s);
}

pub fn print_hex(n: usize) {
    serial_print("0x");
    let digits = b"0123456789ABCDEF";
    for i in (0..core::mem::size_of::<usize>() * 2).rev() {
        let digit = (n >> (i * 4)) & 0xF;
        unsafe { serial_write_byte(digits[digit]); }
    }
}

// ── Kernel main ───────────────────────────────────────────────────────────────

#[repr(C, align(16))]
struct Stack<const N: usize>([u8; N]);

#[no_mangle]
static mut EARLY_STACK: Stack<0x10000> = Stack([0; 0x10000]);

extern "C" {
    fn _start() -> !;
}

#[no_mangle]
pub extern "C" fn kernel_main(boot_info_addr: usize) -> ! {
    unsafe {
        core::ptr::read_volatile(&raw const LIMINE_BASE_REVISION as *const _);
        core::ptr::read_volatile(&raw const ENTRY_POINT_REQUEST as *const _);
        core::ptr::read_volatile(&raw const HHDM_REQUEST as *const _);
        core::ptr::read_volatile(&raw const MEMMAP_REQUEST as *const _);
        core::ptr::read_volatile(&raw const FRAMEBUFFER_REQUEST as *const _);
        core::ptr::read_volatile(&raw const RSDP_REQUEST as *const _);
        core::ptr::read_volatile(&raw const MODULE_REQUEST as *const _);
        early_serial_init();
    }
    serial_print("\n[CYANOS] Kernel starting...\n");

    serial_print("[INIT] Parsing boot info...\n");
    let mut boot_info = if boot_info_addr == 0 {
        unsafe { boot::limine::parse() }
    } else {
        unsafe { boot::multiboot2::parse(boot_info_addr) }
    };
    
    // Store global pointer for syscalls
    unsafe { BOOT_INFO_PTR = core::ptr::addr_of!(boot_info) as usize; }

    if boot_info_addr == 0 {
        unsafe {
            let resp_ptr = core::ptr::read_volatile(ENTRY_POINT_REQUEST.response.get());
            if !resp_ptr.is_null() {
                serial_print("  DEBUG: ENTRY_POINT_REQUEST satisfied!\n");
            } else {
                serial_print("  DEBUG: ENTRY_POINT_REQUEST NOT satisfied\n");
            }
        }

        unsafe {
            let resp_ptr = core::ptr::read_volatile(MODULE_REQUEST.response.get());
            if !resp_ptr.is_null() {
                let resp = &*resp_ptr;
                serial_print("  DEBUG: MODULE_REQUEST satisfied! count: ");
                print_number(resp.module_count as u32);
                serial_print("\n");
                if resp.module_count > 0 {
                    let module = &**resp.modules;
                    // Limine provides virtual address in HHDM; convert to physical.
                    boot_info.initrd_base = (module.address as u64).saturating_sub(boot_info.hhdm_offset);
                    boot_info.initrd_size = module.size;
                }
            } else {
                serial_print("  DEBUG: MODULE_REQUEST NOT satisfied\n");
            }
        }

        let resp_ptr = unsafe { core::ptr::read_volatile(HHDM_REQUEST.response.get()) };
        if !resp_ptr.is_null() {
            let resp = unsafe { &*resp_ptr };
            boot_info.hhdm_offset = resp.offset;
            serial_print("  HHDM Offset: ");
            print_hex(boot_info.hhdm_offset as usize);
            serial_print("\n");
        } else {
            serial_print("  WARNING: HHDM Request NOT satisfied, using fallback\n");
            boot_info.hhdm_offset = 0xffff800000000000;
        }

        #[cfg(target_arch = "x86_64")]
        unsafe { arch_x86_64::apic::set_hhdm_offset(boot_info.hhdm_offset); }
    }

    serial_print("[INIT] Architecture-specific init...\n");
    #[cfg(target_arch = "x86_64")]
    arch_x86_64::init(&boot_info);
    #[cfg(target_arch = "aarch64")]
    arch_aarch64::init(&boot_info);

    serial_print("[INIT] Memory management init...\n");
    mm::init_with_map(boot_info.memory_regions(), boot_info.hhdm_offset as usize);

    serial_print("[INIT] Scheduler init...\n");
    sched::init();

    serial_print("[INIT] Spawning init task...\n");
    init::init_task_main(&boot_info);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_print("\n[CYANOS] KERNEL PANIC: ");
    if let Some(msg) = info.message().as_str() {
        serial_print(msg);
    }
    loop { core::hint::spin_loop(); }
}
