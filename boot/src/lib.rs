//! Boot protocol — structures and parsers shared between bootloader and kernel.
//!
//! On x86-64: multiboot2 (GRUB/QEMU) fills a BootInfo via `multiboot2::parse`.
//! On AArch64: device tree (QEMU/U-Boot) fills a BootInfo via `device_tree::parse`.

#![no_std]

pub mod acpi;
pub mod device_tree;
pub mod limine;
pub mod multiboot2;

// ── Memory map types ─────────────────────────────────────────────────────────

/// Physical memory region types (multiboot2 §3.6.8 / UEFI MemoryType).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryType {
    Available       = 1,
    Reserved        = 2,
    AcpiReclaimable = 3,
    AcpiNvs         = 4,
    BadMemory       = 5,
}

/// A single entry in the physical memory map.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    pub base:   u64,
    pub length: u64,
    pub kind:   MemoryType,
}

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
}

// SAFETY: the BootInfo struct is set up once by the entry stub before any
// secondary CPUs start, and never mutated afterward.
unsafe impl Send for BootInfo {}
unsafe impl Sync for BootInfo {}

impl BootInfo {
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
