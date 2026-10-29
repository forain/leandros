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
    pub const SLOT_ALIGN: u64 = 0x20_0000;
    /// Slots 126-127 are reserved for ktest / testblob.
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
    pub abi_version: u32,
    pub owner: KmodId,
    pub _pad: u16,
    pub cmdline: *const u8,
    pub cmdline_len: usize,
}

#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegKind {
    Kthread = 1,
    Netdev = 2,
    Hwrng = 3,
    UsbClass = 4,
    V4l2 = 5,
    Irq = 6,
    Pci = 7,
}

/// Export-table entry; `name` is NUL-terminated.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KSym {
    pub name: *const u8,
    pub addr: *const (),
}
unsafe impl Sync for KSym {}

/// The kapi extern list is defined by L3c.
pub mod sys {}

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
    assert!(core::mem::size_of::<KmodCtx>() == 24);
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
    assert!(core::mem::offset_of!(KmodCtx, owner) == 4);
    assert!(core::mem::offset_of!(KmodCtx, cmdline) == 8);
    assert!(core::mem::offset_of!(KmodCtx, cmdline_len) == 16);
    assert!(layout::x86_64::BASE % layout::SLOT_ALIGN == 0);
    assert!(layout::aarch64::BASE % layout::SLOT_ALIGN == 0);
};
