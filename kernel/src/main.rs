//! Leandros kernel entry point.

#![no_std]
#![no_main]

extern crate alloc;

mod init;
mod syscall;
mod mem;

// Physical MMIO address of the PL011 the entry stub's EARLY_PUTC markers write
// to, mirroring `arch_aarch64::uart::BASE`. Emitted as an absolute assembler
// symbol ahead of the stub so the stub itself needs no cfg of its own.
//
// The markers run with the MMU off (physical addressing) and again immediately
// after it comes on, where the identity half of `early_pgtables` covers them —
// which is why the entry stub maps the BCM2712 MMIO blocks itself rather than
// waiting for `paging::map_4k`.
// EARLY_LOWMEM_DESC is the block descriptor the entry stub installs for
// physical 0..1GiB. 0x701 is Normal memory, 0x705 is Device (MAIR index 1) --
// the two differ only in the attribute index at bits [4:2]. QEMU virt has
// nothing but MMIO down there, so Device is right; a Pi has ordinary RAM there
// and now runs the kernel image itself from it, where Device would be fatal:
// instruction fetch from a Device mapping is not architecturally permitted.
//
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
core::arch::global_asm!(
    ".set EARLY_UART_BASE, 0x107D001000",
    ".set EARLY_LOWMEM_DESC, 0x701",   // RAM: the kernel itself lives at 0x200000
);
#[cfg(all(target_arch = "aarch64", feature = "raspi4b"))]
core::arch::global_asm!(
    ".set EARLY_UART_BASE, 0xFE201000",
    ".set EARLY_LOWMEM_DESC, 0x705",   // BCM2711 peripherals
);
#[cfg(all(target_arch = "aarch64", not(any(feature = "rpi5", feature = "raspi4b"))))]
core::arch::global_asm!(
    ".set EARLY_UART_BASE, 0x09000000",
    ".set EARLY_LOWMEM_DESC, 0x705",   // QEMU virt: 0..1GiB is all MMIO
);
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
#[no_mangle]
#[link_section = ".limine_reqs"]
pub static FRAMEBUFFER_REQUEST: limine::request::FramebufferRequest = limine::request::FramebufferRequest::new();

#[used]
#[link_section = ".limine_reqs"]
static MODULE_REQUEST: limine::request::ModulesRequest = limine::request::ModulesRequest::new();

#[used]
#[link_section = ".limine_reqs"]
static RSDP_REQUEST: limine::request::RsdpRequest = limine::request::RsdpRequest::new();

#[used]
#[no_mangle]
#[link_section = ".limine_reqs"]
pub static KERNEL_ADDR_REQUEST: limine::request::ExecutableAddressRequest = limine::request::ExecutableAddressRequest::new();

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
    framebuffer_size:    0,
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

    // Mirror every console byte into the active VT's screen buffer before the
    // enabled gate below: an off-screen VT must keep accumulating its text, or
    // switching to it later shows a stale screen.
    tty_server::vt::console_out(b);

    if !KERNEL_CONSOLE_ENABLED.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }

    // Use a per-CPU re-entrancy guard to avoid deadlocks/character loss
    static IN_WRITE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    
    if !IN_WRITE.swap(true, core::sync::atomic::Ordering::SeqCst) {
        drivers::framebuffer::fb_putc(b);
        // Flushing here is what keeps a lone character — a shell prompt, a
        // keystroke echo — visible without waiting for a newline. It is NOT
        // cheap: each call is two synchronous virtqueue round-trips to the host.
        // Callers that already hold a whole buffer take a
        // `framebuffer::FlushBatch` so this collapses to one flush per write()
        // instead of one per byte; see `serial_print` and `serial_write_raw`.
        // On x86 the framebuffer is a host-visible linear surface and fb_flush
        // is a no-op.
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

/// Serial write for the Ctrl-T task dump only (`sched::dump_tasks`) — see
/// `arch_x86_64::putc_dump` / `arch_aarch64::uart::putc_dump` for why this
/// bypasses the sticky TX_WEDGED drop latch that `arch_serial_putc` uses.
#[no_mangle]
pub unsafe extern "C" fn arch_serial_putc_dump(c: u8) {
    #[cfg(target_arch = "x86_64")]
    arch_x86_64::putc_dump(c);
    #[cfg(target_arch = "aarch64")]
    arch_aarch64::uart::putc_dump(c);
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

/// Console size in character cells, for the TTY server's `TIOCGWINSZ`.
///
/// The framebuffer is the primary console, so its cell grid is what programs
/// are told about — a line editor that believes the screen is 80x24 while the
/// framebuffer is much wider wraps and positions against the wrong geometry.
/// Falls back to 80x24 before the framebuffer is initialised.  Lives here
/// rather than in servers/tty because that crate does not depend on `drivers`;
/// this mirrors the existing `kernel_set_console_enabled` callback.
#[no_mangle]
pub extern "C" fn kernel_console_winsize(rows: *mut u16, cols: *mut u16) {
    let (c, r) = drivers::framebuffer::fb_console_size().unwrap_or((80, 24));
    if !rows.is_null() { unsafe { *rows = r as u16; } }
    if !cols.is_null() { unsafe { *cols = c as u16; } }
}

#[no_mangle]
pub extern "C" fn kernel_set_console_enabled(enabled: bool) {
    KERNEL_CONSOLE_ENABLED.store(enabled, core::sync::atomic::Ordering::SeqCst);
    if drivers::pci::RENDER_DEBUG {
        serial_print_str("[KERN] Console enabled = ");
        crate::print_number(if enabled { 1 } else { 0 });
        serial_print_str("\n");
    }
}
/// Serialises whole console writes so they reach the UART uninterrupted.
///
/// `serial_write_byte` talks to the UART one byte at a time with no interlock,
/// so two writers on two vCPUs interleave *character by character*. Userspace
/// log lines are full of SGR escapes, and a splice fabricates control sequences
/// nobody emitted — a captured log contains a literal `ESC [ 3 i` (ANSI media
/// copy) built from brush's `ESC[38;5;` prompt colour and the `i` of a kernel
/// trace's `/bin/brush`, which makes the host terminal open a print dialog.
pub static CONSOLE_OUT_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Run `f` holding [`CONSOLE_OUT_LOCK`], but never block indefinitely.
///
/// Kernel printing happens in interrupt context, where waiting on a lock a
/// preempted thread holds would wedge the CPU. Past the spin budget we print
/// anyway: a rare splice is a far better failure than a dead console.
fn with_console_lock(f: impl FnOnce()) {
    const SPIN_BUDGET: u32 = 100_000;
    let mut spins: u32 = 0;
    let guard = loop {
        if let Some(g) = CONSOLE_OUT_LOCK.try_lock() { break Some(g); }
        spins += 1;
        if spins >= SPIN_BUDGET { break None; }
        core::hint::spin_loop();
    };
    f();
    drop(guard);
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
    with_console_lock(|| {
        let _batch = drivers::framebuffer::FlushBatch::new();
        for &b in bytes {
            serial_write_byte(b);
        }
    });
}

#[no_mangle]
pub extern "C" fn serial_print_str_raw(s: *const u8, len: usize) {
    serial_print(s, len);
}

pub fn serial_print_str(msg: &str) {
    with_console_lock(|| {
        let _batch = drivers::framebuffer::FlushBatch::new();
        for &b in msg.as_bytes() { serial_write_byte(b); }
    });
}

/// Unlocked primitive. Callers that need atomicity take [`CONSOLE_OUT_LOCK`]
/// themselves — `console_write_user` does, and must not re-enter it here
/// (`spin::Mutex` is not reentrant).
pub fn serial_write_raw(bytes: &[u8]) {
    let _batch = drivers::framebuffer::FlushBatch::new();
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
                    framebuffer_size: 0,
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

                // Never probe PCIe on a real Pi 5.
                //
                // Unlike raspi4b (which has no ECAM window at all, so the
                // existing cfg below was enough), BCM2712 does publish a
                // `pcie@120000` node and `device_tree::parse` duly hands back an
                // ECAM base. But the controller needs link training and BAR
                // setup this kernel does not do, so scanning it reads unbacked
                // Device memory — visible in the boot log as a long run of
                // 0xDEAD/0xD0D0 vendor IDs.
                //
                // Those reads raise external aborts, and on this SoC they are
                // reported *asynchronously*. DAIF.A is masked for all of early
                // boot, so nothing happens until the scheduler unmasks to enter
                // userspace, at which point the whole batch arrives at once as
                // `[EXC] Unexpected Exception! ESR=0xBE000011` with ELR pointing
                // at the userspace entry — implicating init, which is innocent.
                //
                // Zeroing the base makes drivers/src/pci.rs take its
                // ecam_base == 0 early return. It costs the RP1 devices behind
                // PCIe (USB, ethernet), none of which this kernel can drive yet.
                #[cfg(feature = "rpi5")]
                { BOOT_INFO.pci_ecam_base = 0; }

                // QEMU `-kernel` provides neither a DTB nor ACPI for a bare ELF,
                // so the device-tree scan above finds nothing and UART/ECAM stay
                // zero. Fall back to the fixed QEMU virt MMIO addresses: the PL011
                // UART is always at 0x0900_0000, and with `-cpu max` (48-bit PA)
                // the PCIe ECAM is the high window at 0x40_1000_0000 — the same
                // base the Limine path discovers via ACPI MCFG. Without this the
                // PCI bus scan finds no devices (e.g. virtio-sound).
                //
                // This fallback is `virt`-machine-specific and must not apply to
                // rpi5/raspi4b builds: `uart::BASE` there is a compile-time
                // constant this dynamic value never overrides (see
                // arch/aarch64/src/uart.rs), and raspi4b (confirmed via QMP
                // `info mtree`) has no PCIe/ECAM region anywhere in its memory
                // map at all — mapping and probing this phantom address would
                // hit genuinely unbacked physical memory instead of the safe
                // ecam_base==0 early-return in drivers/src/pci.rs.
                if BOOT_INFO.uart_base == 0 { BOOT_INFO.uart_base = 0x0900_0000; }
                #[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
                { if BOOT_INFO.pci_ecam_base == 0 { BOOT_INFO.pci_ecam_base = 0x40_1000_0000; } }
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
            // Must match KERNEL_PHYS in whichever direct linker script built
            // this kernel: linkers/aarch64-direct.ld for QEMU virt, and the
            // 0x0020_0000 variant build-all.sh derives from it for --rpi5.
            #[cfg(feature = "rpi5")]
            const KERNEL_PHYS: usize = 0x0020_0000;
            #[cfg(not(feature = "rpi5"))]
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
            // Both boards keep the initrd clear of the kernel image, but the
            // Pi's has to stay inside the low 1GiB the VideoCore can actually
            // write during boot -- 0x4800_0000 gets the same silent refusal the
            // kernel did at 0x4008_0000. 0x1000_0000 sits above the image
            // (which ends around 0x0160_0000) and below the firmware's
            // framebuffer at 0x3f80_0000. Keep in sync with the `initramfs`
            // line scripts/prepare-rpi5-sdcard.sh writes into config.txt.
            #[cfg(feature = "rpi5")]
            const INITRD_PHYS: usize = 0x1000_0000;
            #[cfg(not(feature = "rpi5"))]
            const INITRD_PHYS: usize = 0x4800_0000;
            let initrd_len = init::cpio_image_size(INITRD_PHYS);
            if initrd_len > 0 {
                mm::buddy::reserve_range(INITRD_PHYS, INITRD_PHYS + initrd_len);
                unsafe {
                    BOOT_INFO.initrd_base = INITRD_PHYS as u64;
                    BOOT_INFO.initrd_size = initrd_len as u64;
                }
            }

            // ── VideoCore framebuffer (Raspberry Pi / BCM2712, BCM2711) ──────
            //
            // The Pi 5 firmware publishes no `simple-framebuffer` DTB node — its
            // `fb` node is `brcm,bcm2708-fb` with nothing but a `firmware`
            // phandle — so `device_tree::parse` above leaves framebuffer_base at
            // 0 and the board has been serial-only. Ask the firmware directly.
            //
            // ## Why exactly here
            //
            // *After* the DTB parse, which this needs nothing from but must not
            // disturb.
            //
            // *Before* `mm::init_with_map` (below), because that calls
            // `buddy::init_from_map`, and `mm::buddy::reserve_range` is
            // documented as usable only before it. Getting the reservation in is
            // not optional: the firmware's framebuffer very plausibly sits
            // inside a region the DTB advertises as ordinary available RAM, and
            // an allocator that hands those frames to the heap both scribbles on
            // the screen and loses whatever the heap put there. Two slots of
            // MAX_RESERVED = 8 are used above; this is the third.
            //
            // *Before* `arch_aarch64::init`, which already maps
            // `BOOT_INFO.framebuffer_base` at 0xFFFF_A000_0000_0000 with
            // ATTR_NORMAL_NC and already mirrors the pitch fallback below. That
            // virtual base resolves through L0 slot 320, which `entry_aarch64.s`
            // leaves empty (it populates only 0, 256 and 511) — so `map_4k`
            // builds real 4 KiB leaf entries there rather than colliding with a
            // 1 GiB block descriptor it cannot split. Nothing in that function
            // needs to change.
            //
            // The buffer this hands the firmware has to be physically
            // contiguous, 16-byte aligned, under the 1 GiB VideoCore bus alias,
            // and coherent with a device that reads around our caches. At this
            // point in boot there is no buddy allocator, no heap, and no
            // Normal-NonCacheable MAIR attribute (`mmu::enable_identity`
            // installs index 2 later, inside `arch_aarch64::init`), so it is a
            // cache-line-aligned static in the kernel image plus explicit
            // `dc civac` maintenance. See drivers/src/rpi_mailbox.rs.
            #[cfg(any(feature = "rpi5", feature = "raspi4b"))]
            unsafe {
                // Install VBAR_EL1 before the first MMIO touch of a peripheral
                // whose address is a *deduction* rather than something this
                // kernel has already booted on.
                //
                // `arch_aarch64::init` does this too, but not until after the
                // mailbox call below, and until it runs a fault at EL1 vectors
                // to whatever VBAR_EL1 happens to hold — which is nothing, so
                // the machine wedges in total silence. Proven, not assumed:
                // pointing MBOX_BASE_PHYS at an unbacked address under QEMU
                // raspi4b produced exactly one line of output ("[MBOX] base=")
                // and then two minutes of nothing, because an external abort on
                // unassigned MMIO faulted before any timeout could fire. With
                // vectors installed the same run reports ESR and FAR instead.
                //
                // On a board whose SD card has to be physically moved between
                // machines to reflash, the difference between "silence" and
                // "ESR=…, FAR=0x107C013880" is an entire round trip. `init` is
                // a single `msr vbar_el1` against a static table with no
                // dependencies, and calling it twice writes the same value.
                arch_aarch64::exception::init();

                if let Some((s, e)) = drivers::rpi_mailbox::scratch_reservation() {
                    // raspi4b only: that build links at physical 0x4008_0000,
                    // above the bus alias, so the property buffer cannot live in
                    // the image and uses a fixed low scratch page instead.
                    mm::buddy::reserve_range(s, e);
                }
                match drivers::rpi_mailbox::init_framebuffer(hhdm_offset as usize, 1920, 1080) {
                    Ok(fb) => {
                        BOOT_INFO.framebuffer_base = fb.phys;
                        BOOT_INFO.framebuffer_width = fb.width;
                        BOOT_INFO.framebuffer_height = fb.height;
                        BOOT_INFO.framebuffer_pitch = fb.pitch;
                        // The WHOLE allocation, off-screen panning rows
                        // included — `reserve_len()` is `virt_height`-based on
                        // purpose. Reserving only the visible `pitch * height`
                        // would hand the buddy allocator the half of the buffer
                        // the next lane wants to pan into.
                        let len = fb.reserve_len() as usize;
                        // Tells `arch_aarch64::init` to map the whole buffer,
                        // not just the visible `pitch * height`. Without it the
                        // off-screen panning rows are reserved but unmapped —
                        // caught on the very first QEMU raspi4b run as a level-2
                        // translation fault at FAR = fb_virt + size - 4.
                        BOOT_INFO.framebuffer_size = len as u64;
                        mm::buddy::reserve_range(fb.phys as usize, fb.phys as usize + len);
                        arch_aarch64::uart::serial_print_str("[MBOX] reserved ");
                        arch_aarch64::uart::print_hex(fb.phys as usize);
                        arch_aarch64::uart::serial_print_str("-");
                        arch_aarch64::uart::print_hex(fb.phys as usize + len);
                        arch_aarch64::uart::serial_print_str(" virt_height=");
                        crate::print_number(fb.virt_height);
                        arch_aarch64::uart::serial_print_str("\n");
                    }
                    Err(_) => {
                        // Already logged in detail by the driver. Leaving
                        // framebuffer_base at 0 drops through to kernel_main's
                        // existing "no bootloader framebuffer" branch and a
                        // serial-only boot — bit for bit what this board does
                        // today, so the failure path is already proven.
                    }
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
                // Real Raspberry Pi 5 firmware always hands off a DTB, so this fallback is
                // only ever reached under QEMU (direct boot with no -dtb, or UEFI+ACPI-only
                // boot) — a --rpi5 build must still land here with a QEMU-valid address,
                // not the real hardware's UART MMIO address, or it hard-hangs in QEMU.
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
        // The whole map, once: which physical ranges the buddy owns and
        // which it must never touch is the first question of every
        // memory-corruption post-mortem, and it changes with guest RAM size.
        for r in unsafe { (*core::ptr::addr_of!(BOOT_INFO)).memory_regions() } {
            serial_print_str("[MEMMAP] ");
            serial_print_hex(r.base as usize);
            serial_print_str(" +");
            serial_print_hex(r.length as usize);
            serial_print_str(match r.kind {
                boot::MemoryType::Available       => " usable\n",
                boot::MemoryType::AcpiReclaimable => " acpi-reclaimable\n",
                boot::MemoryType::AcpiNvs         => " acpi-nvs\n",
                boot::MemoryType::BadMemory       => " bad\n",
                _                                 => " reserved\n",
            });
        }
        serial_print_str("[MEMMAP] buddy pages total=");
        serial_print_hex(mm::buddy::total_pages());
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
        // The whole map, once: which physical ranges the buddy owns and
        // which it must never touch is the first question of every
        // memory-corruption post-mortem, and it changes with guest RAM size.
        for r in unsafe { (*core::ptr::addr_of!(BOOT_INFO)).memory_regions() } {
            serial_print_str("[MEMMAP] ");
            serial_print_hex(r.base as usize);
            serial_print_str(" +");
            serial_print_hex(r.length as usize);
            serial_print_str(match r.kind {
                boot::MemoryType::Available       => " usable\n",
                boot::MemoryType::AcpiReclaimable => " acpi-reclaimable\n",
                boot::MemoryType::AcpiNvs         => " acpi-nvs\n",
                boot::MemoryType::BadMemory       => " bad\n",
                _                                 => " reserved\n",
            });
        }
        serial_print_str("[MEMMAP] buddy pages total=");
        serial_print_hex(mm::buddy::total_pages());
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

            // Same accessor `arch_aarch64::init` mapped with, so the console
            // can never draw against a stride the mapping did not cover.
            let pitch_bytes = bi.framebuffer_pitch_bytes() as u32;

            vfs_server::set_framebuffer(bi.framebuffer_base, width, height, pitch_bytes);
            
            // Also set it in the framebuffer driver for KMS integration
            drivers::framebuffer::set_boot_framebuffer(bi.framebuffer_base, width, height, pitch_bytes);

            // Verify it was set
            if drivers::framebuffer::get_hardware_fb_info().is_some() {
                serial_print_str("[MAIN] BOOT_FB registered successfully\n");
            } else {
                serial_print_str("[MAIN] BOOT_FB registration FAILED\n");
            }
            let fb_virt = if cfg!(target_arch = "aarch64") {
                0xFFFF_A000_0000_0000 + (bi.framebuffer_base as usize & 4095)
            } else {
                mm::phys_to_virt(bi.framebuffer_base as usize)
            };
            serial_print_str("[MAIN] Framebuffer virtual address: ");
            serial_print_hex(fb_virt);
            serial_print_str("\n");

            // ── Raspberry Pi framebuffer: evict the cacheable aliases ────────
            //
            // The surface is mapped Normal-NonCacheable at fb_virt, but the same
            // physical pages are ALSO covered by the 1 GiB Normal Write-Back
            // block descriptors `entry_aarch64.s` installs — once identity
            // (TTBR0, L0[0]) and once through the HHDM (TTBR1, L0[256]). Nothing
            // reads or writes the framebuffer through those aliases, which is
            // what makes the mismatched attributes survivable, but a line pulled
            // in speculatively before the reservation took effect would shadow
            // the first frame. One `dc civac` sweep forecloses that whole class
            // of "stale pixels on the one hardware boot" for well under a
            // millisecond: 8.3 MB at a 64-byte line is ~130k operations.
            //
            // Covers RPI_FB_RESERVE_LEN, not `pitch * height` — the allocation
            // may be twice as tall as the visible area.
            //
            // Runs here rather than in `arch_aarch64::init`, which maps the
            // framebuffer *before* `exception::init` installs VBAR_EL1; a fault
            // there would be a silent hang with no vectors to report it.
            #[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
            {
                // Identical accessor to the one `arch_aarch64::init` mapped
                // with — the sweep can no longer outrun the mapping.
                let sweep = bi.framebuffer_bytes();
                arch_aarch64::arch_dcache_clean_inval_range(fb_virt, sweep);
                serial_print_str("[MBOX] dcache sweep ");
                serial_print_hex(sweep);
                serial_print_str("\n");

                // Readback probe: prove the mapping lands on real, writable
                // memory before the console starts trusting it. A wrong
                // bus→physical conversion produces a mapping that reads back
                // something other than what was written (or faults), and this
                // says so in one line instead of costing a second boot.
                let p = fb_virt as *mut u32;
                let last = (sweep / 4).saturating_sub(1);
                p.write_volatile(0xA5A5_5A5A);
                p.add(last).write_volatile(0x5A5A_A5A5);
                core::arch::asm!("dsb sy", options(nostack, preserves_flags));
                serial_print_str("[MBOX] fbrb first=");
                serial_print_hex(p.read_volatile() as usize);
                serial_print_str(" last=");
                serial_print_hex(p.add(last).read_volatile() as usize);
                serial_print_str("\n");
            }

            drivers::framebuffer::init_kernel_fb(
                fb_virt as *mut u32,
                width as usize,
                height as usize,
                pitch_bytes as usize,
            );

            // ── Pixel-order reference bars ───────────────────────────────────
            //
            // Red, green and blue in vertical thirds across the top of the
            // screen, painted straight into the surface after the console has
            // cleared it, and held long enough to photograph.
            //
            // `SET_PIXEL_ORDER` takes 0 for BGR and 1 for RGB, and which one
            // matches this console is the single thing in the whole design that
            // serial output cannot settle: `drivers::framebuffer` composes
            // colours as 0x00RRGGBB, and whether the firmware scans those bytes
            // out as red-first depends on a value we had to guess. A photograph
            // settles it. The Pi 5's SD card has to be physically moved between
            // machines to reflash, so three seconds of boot time is a very cheap
            // substitute for a second round trip. Set the hold to 0 to disable.
            //
            // Vertical thirds rather than horizontal bands so left-to-right
            // order is unambiguous in the photo; the console keeps drawing over
            // them from the top-left as boot proceeds, which is fine.
            #[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
            {
                const BAR_HOLD_SECS: u64 = 3;
                const BAR_ROWS: usize = 128;
                let stride = pitch_bytes as usize / 4;
                let p = fb_virt as *mut u32;
                let w = width as usize;
                let rows = if (height as usize) < BAR_ROWS { height as usize } else { BAR_ROWS };
                for y in 0..rows {
                    for x in 0..w {
                        let c = if x < w / 3 {
                            0x00FF_0000 // red
                        } else if x < 2 * w / 3 {
                            0x0000_FF00 // green
                        } else {
                            0x0000_00FF // blue
                        };
                        p.add(y * stride + x).write_volatile(c);
                    }
                }
                core::arch::asm!("dsb sy", options(nostack, preserves_flags));
                serial_print_str("[MBOX] colour bars: RED GREEN BLUE left-to-right, holding\n");
                if BAR_HOLD_SECS > 0 {
                    let f: u64;
                    core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack));
                    let hz = if f == 0 { 100_000_000 } else { f };
                    let start: u64;
                    core::arch::asm!("mrs {}, cntvct_el0", out(reg) start, options(nomem, nostack));
                    loop {
                        let now: u64;
                        core::arch::asm!("mrs {}, cntvct_el0", out(reg) now, options(nomem, nostack));
                        if now.wrapping_sub(start) >= hz * BAR_HOLD_SECS { break; }
                        core::hint::spin_loop();
                    }
                }
            }
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
        
        // Bring up the VT layer now that the framebuffer console (bootloader
        // or VirtIO GPU) has had its chance to init — covers both branches
        // above, and is harmless if neither produced a framebuffer.
        tty_server::vt::init();

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
    // Take the framebuffer back before printing anything. The console yields
    // the scanout to a DRM master (drivers/src/framebuffer.rs), and nothing is
    // going to give it back, so a panic during a graphical session would reach
    // the serial line and nothing else. Two atomic stores, no locks — the
    // panicking thread may already hold KERNEL_FB.
    drivers::framebuffer::console_force_reclaim();
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
