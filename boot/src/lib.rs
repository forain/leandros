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

    pub fn total_available_memory(&self) -> u64 {
        self.memory_regions()
            .iter()
            .filter(|r| r.kind == MemoryType::Available)
            .map(|r| r.length)
            .sum()
    }
}
