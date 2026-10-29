//! Frozen kmod ABI v1. See docs/design (kmod L3b). Any change bumps
//! `KMOD_ABI_VERSION`.
#![no_std]

pub const KMOD_MAGIC: [u8; 8] = *b"LKMOD\0\0\x01";
pub const KMOD_ABI_VERSION: u32 = 1;

pub const EM_X86_64: u32 = 62;
pub const EM_AARCH64: u32 = 183;

pub const FLAG_SOFTFLOAT_CHECKED: u32 = 1 << 0;
pub const FLAG_PERMANENT: u32 = 1 << 1;

/// Module window constants. All window constants live here.
pub mod layout {
    pub mod x86_64 {
        pub const BASE: u64 = 0xffff_ffff_9000_0000;
    }
    pub mod aarch64 {
        pub const BASE: u64 = 0xffff_fe00_0000_0000;
    }
    pub const SIZE: u64 = 0x1000_0000;
    pub const SLOT_ALIGN: u64 = 0x4_0000; // 256 KiB, 1024 slots
    /// Slots 1022-1023 are reserved for ktest / testblob.
    pub const TEST_SLOT_OFF: u64 = SIZE - 2 * SLOT_ALIGN;
    pub const NSLOTS: u64 = SIZE / SLOT_ALIGN;

    #[cfg(target_arch = "x86_64")]
    pub use x86_64::BASE;
    #[cfg(target_arch = "aarch64")]
    pub use aarch64::BASE;
}

/// Lives at `slot_base`, the first bytes of the first PT_LOAD (perm R).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KmodHeader {
    pub magic: [u8; 8],      // 0
    pub abi_version: u32,    // 8
    pub header_size: u32,    // 12 == 128
    pub machine: u32,        // 16 EM_X86_64 | EM_AARCH64
    pub flags: u32,          // 20
    pub build_id: [u8; 20],  // 24
    pub build_id_len: u32,   // 44 == 20
    pub slot_base: u64,      // 48
    pub slot_size: u64,      // 56
    pub init: u64,           // 64 VA of extern "C" fn(*const KmodCtx) -> i32
    pub exit: u64,           // 72 0 in v1
    pub ids: u64,            // 80
    pub ids_len: u32,        // 88
    pub ids_entry_size: u32, // 92
    pub name: [u8; 32],      // 96 NUL-padded [a-z0-9_]
}

pub const HEADER_SIZE: usize = 128;

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KmodId(pub u16); // 0 = BUILTIN

#[repr(C)]
pub struct KmodCtx {
    pub ctx_size: u32,    // 0, == size_of::<KmodCtx>()
    pub abi_version: u32, // 4
    pub owner: KmodId,    // 8
    pub _pad: u16,        // 10
    pub _pad2: u32,       // 12
    pub cmdline: *const u8, // 16
    pub cmdline_len: usize, // 24
    pub params: *const u8,  // 32, init_module params (ignored in v1)
    pub params_len: usize,  // 40
}

/// Registration kind. A struct, not an enum: an out-of-range value from a
/// module across FFI must not be UB.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegKind(pub u16);
impl RegKind {
    pub const KTHREAD: RegKind = RegKind(1);
    pub const NETDEV: RegKind = RegKind(2);
    pub const HWRNG: RegKind = RegKind(3);
    pub const USB_CLASS: RegKind = RegKind(4);
    pub const V4L2: RegKind = RegKind(5);
    pub const IRQ: RegKind = RegKind(6);
    pub const PCI: RegKind = RegKind(7);
}

/// Device-id table entry (`.kmod.ids`), 16 bytes. PCI: a=vendor, b=device (0xffff = any).
/// USB: a=vid, b=pid (0xffff = any), class = class<<16|sub<<8|proto under `class_mask`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KmodDevId {
    pub bus: u16,
    pub a: u16,
    pub b: u16,
    pub c: u16,
    pub class: u32,
    pub class_mask: u32,
}

/// PCI device record returned by `kapi_pci_find` (40 bytes: the plan said 48, but the listed fields sum to 40). `bdf` = bus<<16|dev<<8|func,
/// `class` = class<<16|subclass<<8|prog_if.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KPciDev {
    pub bdf: u32,
    pub vendor: u16,
    pub device: u16,
    pub class: u32,
    pub _pad: u32,
    pub bars: [u32; 6],
}

/// `kapi_map_device` flags. Never executable, never user.
pub const KMAP_WRITABLE: u32 = 1;
pub const KMAP_MMIO: u32 = 2;
pub const KMAP_WC: u32 = 4;

/// Every id-table entry starts with a `u16 bus` discriminator.
pub const BUS_PCI: u16 = 1;
pub const BUS_USB: u16 = 2;
pub const BUS_OF: u16 = 3;
pub const BUS_ACPI: u16 = 4;

/// Export-table entry; `name` is NUL-terminated.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KSym {
    pub name: *const u8,
    pub addr: *const (),
}
unsafe impl Sync for KSym {}

/// The core kapi v1 imports (implemented by the kernel, `kernel/src/kmod/exports.rs`).
/// The `*Fn` aliases are what the kernel's compile-time signature check uses.
pub mod sys {
    use super::{KmodCtx, KmodId};
    pub type AbiVersionFn = unsafe extern "C" fn() -> u32;
    pub type LogFn = unsafe extern "C" fn(u32, *const u8, usize);
    pub type PanicFn = unsafe extern "C" fn(KmodId, *const u8, usize) -> !;
    pub type AllocFn = unsafe extern "C" fn(usize, usize) -> *mut u8;
    pub type DeallocFn = unsafe extern "C" fn(*mut u8, usize, usize);
    pub type ReallocFn = unsafe extern "C" fn(*mut u8, usize, usize, usize) -> *mut u8;
    pub type CmdlineFn = unsafe extern "C" fn(*mut usize) -> *const u8;
    pub type MonotonicFn = unsafe extern "C" fn() -> u64;
    pub type SleepFn = unsafe extern "C" fn(u64);
    pub type SpinHeldFn = unsafe extern "C" fn(bool);
    pub type SpinHeldCountFn = unsafe extern "C" fn() -> u32;
    pub type KthreadSpawnFn =
        unsafe extern "C" fn(*const u8, usize, extern "C" fn(*mut u8), *mut u8, KmodId) -> i64;
    pub type KtestPingFn = unsafe extern "C" fn(*const KmodCtx) -> i32;
    pub type PciFindFn = unsafe extern "C" fn(u16, u16, u32, *mut super::KPciDev) -> i32;
    pub type PciCfgReadFn = unsafe extern "C" fn(u32, u16, u8) -> u32;
    pub type PciCfgWriteFn = unsafe extern "C" fn(u32, u16, u8, u32);
    pub type MapDeviceFn = unsafe extern "C" fn(u64, usize, u32) -> usize;
    pub type PhysToVirtFn = unsafe extern "C" fn(u64) -> usize;
    pub type PageAllocFn = unsafe extern "C" fn(u32, u64) -> u64;
    pub type PageFreeFn = unsafe extern "C" fn(u64, u32);
    pub type HwrngRegisterFn =
        unsafe extern "C" fn(KmodId, *const u8, usize, extern "C" fn(*mut u8, usize) -> usize) -> i32;

    extern "C" {
        pub fn kapi_abi_version() -> u32;
        pub fn kapi_log(level: u32, ptr: *const u8, len: usize);
        pub fn kapi_panic(owner: KmodId, ptr: *const u8, len: usize) -> !;
        pub fn kapi_alloc(size: usize, align: usize) -> *mut u8;
        pub fn kapi_dealloc(ptr: *mut u8, size: usize, align: usize);
        pub fn kapi_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> *mut u8;
        pub fn kapi_cmdline(len_out: *mut usize) -> *const u8;
        pub fn kapi_monotonic_ns() -> u64;
        pub fn kapi_msleep(ms: u64);
        pub fn kapi_udelay(us: u64);
        pub fn kapi_spin_held(acquired: bool);
        pub fn kapi_spin_held_count() -> u32;
        pub fn kapi_kthread_spawn(name: *const u8, name_len: usize, f: extern "C" fn(*mut u8),
                                  arg: *mut u8, owner: KmodId) -> i64;
        pub fn kapi_ktest_ping(ctx: *const KmodCtx) -> i32;
        pub fn kapi_pci_find(vendor: u16, device: u16, index: u32, out: *mut super::KPciDev) -> i32;
        pub fn kapi_pci_cfg_read(bdf: u32, off: u16, width: u8) -> u32;
        pub fn kapi_pci_cfg_write(bdf: u32, off: u16, width: u8, val: u32);
        pub fn kapi_map_device(phys: u64, len: usize, flags: u32) -> usize;
        pub fn kapi_phys_to_virt(phys: u64) -> usize;
        pub fn kapi_page_alloc(order: u32, below: u64) -> u64;
        pub fn kapi_page_free(phys: u64, order: u32);
        pub fn kapi_hwrng_register(owner: KmodId, name: *const u8, len: usize,
                                   read: extern "C" fn(*mut u8, usize) -> usize) -> i32;
    }
}

impl KmodHeader {
    /// Parse from little-endian bytes (any alignment). None if too short.
    pub fn from_bytes(b: &[u8]) -> Option<KmodHeader> {
        if b.len() < HEADER_SIZE {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        Some(KmodHeader {
            magic: b[0..8].try_into().unwrap(),
            abi_version: u32_at(8),
            header_size: u32_at(12),
            machine: u32_at(16),
            flags: u32_at(20),
            build_id: b[24..44].try_into().unwrap(),
            build_id_len: u32_at(44),
            slot_base: u64_at(48),
            slot_size: u64_at(56),
            init: u64_at(64),
            exit: u64_at(72),
            ids: u64_at(80),
            ids_len: u32_at(88),
            ids_entry_size: u32_at(92),
            name: b[96..128].try_into().unwrap(),
        })
    }

    pub fn write_bytes(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.magic);
        out[8..12].copy_from_slice(&self.abi_version.to_le_bytes());
        out[12..16].copy_from_slice(&self.header_size.to_le_bytes());
        out[16..20].copy_from_slice(&self.machine.to_le_bytes());
        out[20..24].copy_from_slice(&self.flags.to_le_bytes());
        out[24..44].copy_from_slice(&self.build_id);
        out[44..48].copy_from_slice(&self.build_id_len.to_le_bytes());
        out[48..56].copy_from_slice(&self.slot_base.to_le_bytes());
        out[56..64].copy_from_slice(&self.slot_size.to_le_bytes());
        out[64..72].copy_from_slice(&self.init.to_le_bytes());
        out[72..80].copy_from_slice(&self.exit.to_le_bytes());
        out[80..88].copy_from_slice(&self.ids.to_le_bytes());
        out[88..92].copy_from_slice(&self.ids_len.to_le_bytes());
        out[92..96].copy_from_slice(&self.ids_entry_size.to_le_bytes());
        out[96..128].copy_from_slice(&self.name);
    }
}

const _: () = {
    assert!(core::mem::size_of::<KmodHeader>() == 128);
    assert!(core::mem::align_of::<KmodHeader>() == 8);
    assert!(core::mem::size_of::<KmodId>() == 2);
    assert!(core::mem::size_of::<KmodCtx>() == 48);
    assert!(core::mem::size_of::<KSym>() == 16);
    assert!(core::mem::offset_of!(KmodHeader, magic) == 0);
    assert!(core::mem::offset_of!(KmodHeader, abi_version) == 8);
    assert!(core::mem::offset_of!(KmodHeader, header_size) == 12);
    assert!(core::mem::offset_of!(KmodHeader, machine) == 16);
    assert!(core::mem::offset_of!(KmodHeader, flags) == 20);
    assert!(core::mem::offset_of!(KmodHeader, build_id) == 24);
    assert!(core::mem::offset_of!(KmodHeader, build_id_len) == 44);
    assert!(core::mem::offset_of!(KmodHeader, slot_base) == 48);
    assert!(core::mem::offset_of!(KmodHeader, slot_size) == 56);
    assert!(core::mem::offset_of!(KmodHeader, init) == 64);
    assert!(core::mem::offset_of!(KmodHeader, exit) == 72);
    assert!(core::mem::offset_of!(KmodHeader, ids) == 80);
    assert!(core::mem::offset_of!(KmodHeader, ids_len) == 88);
    assert!(core::mem::offset_of!(KmodHeader, ids_entry_size) == 92);
    assert!(core::mem::offset_of!(KmodHeader, name) == 96);
    assert!(core::mem::offset_of!(KmodCtx, abi_version) == 4);
    assert!(core::mem::offset_of!(KmodCtx, owner) == 8);
    assert!(core::mem::offset_of!(KmodCtx, cmdline) == 16);
    assert!(core::mem::offset_of!(KmodCtx, cmdline_len) == 24);
    assert!(core::mem::offset_of!(KmodCtx, params) == 32);
    assert!(core::mem::offset_of!(KmodCtx, params_len) == 40);
    assert!(core::mem::size_of::<RegKind>() == 2);
    assert!(core::mem::size_of::<KmodDevId>() == 16);
    assert!(core::mem::size_of::<KPciDev>() == 40);
    assert!(layout::TEST_SLOT_OFF == layout::SIZE - 2 * layout::SLOT_ALIGN);
    assert!(layout::TEST_SLOT_OFF == 0x0ff8_0000);
    assert!(layout::x86_64::BASE % layout::SLOT_ALIGN == 0);
    assert!(layout::aarch64::BASE % layout::SLOT_ALIGN == 0);
};
