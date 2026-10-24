//! Boot protocol — structures and parsers shared between bootloader and kernel.
//!
//! On x86-64: multiboot2 (GRUB/QEMU) fills a BootInfo via `multiboot2::parse`.
//! On AArch64: device tree (QEMU/U-Boot) fills a BootInfo via `device_tree::parse`.

#![cfg_attr(not(test), no_std)]

pub mod acpi;
pub mod earlyfb;
pub mod device_tree;
pub mod limine;
pub mod multiboot2;
pub mod mb2_build;
pub mod fdt_build;

// ── Memory map types ─────────────────────────────────────────────────────────

pub mod memmap;
pub use memmap::{MemoryRegion, MemoryType};

// ── Unified boot info ─────────────────────────────────────────────────────────

/// Kernel boot information — filled from multiboot2 or DTB, then passed to
/// the rest of the kernel initialisation sequence.
#[repr(C)]
pub struct BootInfo {
    /// Physical memory map (pointer into static storage inside the parser).
    pub memory_map:          *const MemoryRegion,
    pub memory_map_len:      usize,
    /// Linear framebuffer (0 if not present).
    pub framebuffer_base:    u64,
    pub framebuffer_width:   u32,
    pub framebuffer_height:  u32,
    pub framebuffer_pitch:   u32,
    /// Total bytes the framebuffer allocation actually occupies.
    ///
    /// 0 means "not reported" — fall back to `pitch * height`, which is what
    /// every bootloader path does. It is non-zero only when the surface is
    /// larger than the visible area: the Raspberry Pi mailbox asks the
    /// VideoCore firmware for a buffer twice as tall as the display so a later
    /// lane can scroll by panning `SET_VIRTUAL_OFFSET` instead of copying the
    /// whole surface. Those off-screen rows still have to be mapped, reserved
    /// and cache-swept, and `height` does not describe them.
    pub framebuffer_size:    u64,
    /// ACPI Root System Description Pointer (0 if not present).
    pub rsdp_addr:           u64,
    /// UART MMIO base address discovered from DTB (0 if not found / not a DTB boot).
    pub uart_base:           u64,
    /// PCI ECAM base address discovered from DTB (0 if not found).
    pub pci_ecam_base:       u64,
    /// Initrd/ramdisk physical address and size (0 if not present).
    pub initrd_base:         u64,
    pub initrd_size:         u64,
    /// Higher-Half Direct Map virtual offset (0 if not present).
    pub hhdm_offset:         u64,
    /// Kernel command line handed over by a direct (non-Limine) boot: a
    /// pointer to a NUL-free UTF-8 copy in a static buffer inside this crate
    /// (so it outlives the MBI/DTB it came from) and its length. 0 if none.
    pub cmdline_ptr:         u64,
    pub cmdline_len:         u64,
    /// EFI system table physical address (multiboot2 EFI64 tag), 0 if none.
    pub efi_system_table:    u64,
}

/// The previous kernel's final kexec log (`(phys, size)` of the buffer), as
/// a kexec'ing LeandrOS handed it over: multiboot2 tag
/// [`multiboot2::TAG_LEANDROS_PREVLOG`] (x86_64) or
/// `/chosen/leandros,kexec-prevlog` (aarch64). Read by
/// `kernel/src/kexec/prevlog.rs`. (0, 0) = none.
static KEXEC_PREVLOG: [core::sync::atomic::AtomicU64; 2] =
    [core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)];

pub(crate) fn set_kexec_prevlog(phys: u64, size: u64) {
    KEXEC_PREVLOG[0].store(phys, core::sync::atomic::Ordering::Relaxed);
    KEXEC_PREVLOG[1].store(size, core::sync::atomic::Ordering::Relaxed);
}

/// See [`KEXEC_PREVLOG`].
pub fn kexec_prevlog() -> Option<(u64, u64)> {
    let p = KEXEC_PREVLOG[0].load(core::sync::atomic::Ordering::Relaxed);
    let s = KEXEC_PREVLOG[1].load(core::sync::atomic::Ordering::Relaxed);
    if p == 0 || s == 0 { None } else { Some((p, s)) }
}

/// Longest command line kept from a direct boot.
pub const CMDLINE_MAX: usize = 4096;
static mut CMDLINE_BUF: [u8; CMDLINE_MAX] = [0; CMDLINE_MAX];

/// Copy `src` into the static command-line buffer and point `info` at it.
/// Trailing NULs are dropped; an empty line leaves `info` untouched.
///
/// # Safety
/// Boot parsers only: single CPU, before anything else reads the buffer.
pub(crate) unsafe fn stash_cmdline(info: &mut BootInfo, src: &[u8]) {
    let mut n = src.len().min(CMDLINE_MAX);
    // Stop at the first NUL: the source is a C string.
    if let Some(z) = src[..n].iter().position(|&b| b == 0) { n = z; }
    if n == 0 { return; }
    let dst = core::ptr::addr_of_mut!(CMDLINE_BUF) as *mut u8;
    core::ptr::copy_nonoverlapping(src.as_ptr(), dst, n);
    info.cmdline_ptr = dst as u64;
    info.cmdline_len = n as u64;
}

// SAFETY: the BootInfo struct is set up once by the entry stub before any
// secondary CPUs start, and never mutated afterward.
unsafe impl Send for BootInfo {}
unsafe impl Sync for BootInfo {}

impl BootInfo {
    /// The direct-boot command line, if the parser found a non-empty one.
    pub fn cmdline(&self) -> Option<&'static str> {
        if self.cmdline_ptr == 0 || self.cmdline_len == 0 { return None; }
        // SAFETY: points into CMDLINE_BUF, written once by `stash_cmdline`.
        let b = unsafe { core::slice::from_raw_parts(self.cmdline_ptr as *const u8, self.cmdline_len as usize) };
        core::str::from_utf8(b).ok()
    }

    pub fn memory_regions(&self) -> &[MemoryRegion] {
        if self.memory_map.is_null() { return &[]; }
        // SAFETY: pointer and length come from a trusted boot parser.
        unsafe { core::slice::from_raw_parts(self.memory_map, self.memory_map_len) }
    }

    /// Bytes per framebuffer row, with the fallback every caller used to
    /// open-code.
    ///
    /// Some firmware DTBs omit or zero `stride`. A caller that maps or sweeps
    /// using the raw value then covers far less than the console later writes
    /// into, and walks off the mapped region on the first `clear`.
    pub fn framebuffer_pitch_bytes(&self) -> usize {
        let tight = self.framebuffer_width as usize * 4;
        if (self.framebuffer_pitch as usize) < tight {
            tight
        } else {
            self.framebuffer_pitch as usize
        }
    }

    /// **The** definition of how large the framebuffer is, in bytes.
    ///
    /// Three places need this number — the page-table mapping in
    /// `arch_aarch64::init`, the buddy-allocator reservation, and the cache
    /// sweep in `kernel_main` — and when they each computed it independently
    /// they disagreed. The mapping used the *visible* `pitch * height` while
    /// the reservation and sweep used the whole allocation, so a `dc civac`
    /// walked off the end of the mapping and took a level-2 translation fault
    /// at the very last word (`ESR 0x96000046`, `FAR = fb_virt + size - 4`).
    /// The arithmetic was not the bug; having three copies of it was.
    ///
    /// `framebuffer_size` wins when it is larger, which happens only when the
    /// surface is deliberately taller than the display — see that field.
    pub fn framebuffer_bytes(&self) -> usize {
        let geom = self.framebuffer_pitch_bytes() * self.framebuffer_height as usize;
        if (self.framebuffer_size as usize) > geom {
            self.framebuffer_size as usize
        } else {
            geom
        }
    }

    pub fn total_available_memory(&self) -> u64 {
        self.memory_regions()
            .iter()
            .filter(|r| r.kind == MemoryType::Available)
            .map(|r| r.length)
            .sum()
    }
}

// ── Memory map sanitizing (lane fbmm) ────────────────────────────────────────

/// Room for a whole sanitized map plus extra entries (the kernel re-runs the
/// sanitizer on its own output to punch out HHDM holes).
const SAN_IN: usize = memmap::MAX_SANE + 64;
static mut SAN_SCRATCH: [MemoryRegion; SAN_IN] = [MemoryRegion::EMPTY; SAN_IN];
static mut SAN_BOUNDS: [u64; 2 * SAN_IN] = [0; 2 * SAN_IN];
static mut SAN_OUT: [MemoryRegion; memmap::MAX_SANE] = [MemoryRegion::EMPTY; memmap::MAX_SANE];
static mut SAN_STATS: Option<memmap::Stats> = None;
static mut SAN_INPUT_TOTAL: usize = 0;

/// Run `info`'s memory map through [`memmap::sanitize`] and point `info` at
/// the result (static storage). `reported` is how many entries the boot
/// protocol said it had (the parser may have kept fewer); 0 = unknown.
///
/// # Safety
/// Single-threaded early boot only (uses static scratch).
/// `extra` entries (e.g. Reserved holes) are added to the input; the most
/// restrictive kind wins where they overlap.
pub unsafe fn sanitize_boot_info(info: &mut BootInfo, reported: usize, extra: &[MemoryRegion]) -> memmap::Stats {
    let src_len = info.memory_map_len;
    let scratch = &mut *core::ptr::addr_of_mut!(SAN_SCRATCH);
    let mut n = src_len.min(SAN_IN);
    scratch[..n].copy_from_slice(&info.memory_regions()[..n]);
    for e in extra { if n < SAN_IN { scratch[n] = *e; n += 1; } }
    let st = memmap::sanitize(&mut scratch[..n],
        &mut *core::ptr::addr_of_mut!(SAN_BOUNDS),
        &mut *core::ptr::addr_of_mut!(SAN_OUT));
    info.memory_map = core::ptr::addr_of!(SAN_OUT) as *const MemoryRegion;
    info.memory_map_len = st.output;
    SAN_STATS = Some(st);
    SAN_INPUT_TOTAL = reported.max(src_len);
    st
}

/// The last [`sanitize_boot_info`] result and the protocol's own entry count.
pub fn sanitize_stats() -> Option<(memmap::Stats, usize)> {
    unsafe { (*core::ptr::addr_of!(SAN_STATS)).map(|s| (s, SAN_INPUT_TOTAL)) }
}
