//! Platform-device and board-service ABI (kmodpi P0): the device record handed to a module probe,
//! the probe/quiesce callback types and the extern declarations of the plat exports.

use crate::KmodId;

/// One announced platform device. `size` is `size_of::<KPlatDev>()` so fields can be appended.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KPlatDev {
    pub size: u32,
    /// Index into the compat list the module registered, of the string that matched.
    pub compat_idx: u32,
    pub reg: [KPlatReg; 4],
    pub irq: [u32; 4],
    pub nirq: u32,
    pub dma_offset: u64,
    pub flags: u32,
    pub dt_node: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KPlatReg { pub phys: u64, pub len: u64 }

/// Probe: return true to claim the device. Runs in the registering module's init or in the announcer's context.
pub type KPlatProbeFn = extern "C" fn(ctx: usize, dev: *const KPlatDev) -> bool;
/// DMA quiesce hook (kexec stop of a bus master): true when the engine is quiet. Must be bounded and lock-free.
pub type KDmaQuiesceFn = extern "C" fn(ctx: usize) -> bool;

/// `kapi_dma_quiesce_register` stages.
pub mod quiesce_stage { pub const DEVICE: u32 = 0; pub const BUS: u32 = 1; }
/// `kapi_pi5_pins` operations.
pub mod pi5_pins { pub const MUX_SDIO2: u32 = 0; pub const WL_ON_SET: u32 = 1; pub const WL_ON_READY: u32 = 2; }
/// `kapi_board_mac` selectors.
pub mod board_mac { pub const ETH: u32 = 0; pub const WIFI: u32 = 1; pub const BT: u32 = 2; }
/// `kapi_dma_sync` directions beyond 0..2: raw cache maintenance on any mapped VA (also `kapi_vmap` aliases).
pub mod dma_dir { pub const CLEAN: u32 = 3; pub const INVALIDATE: u32 = 4; }

pub mod sys_plat {
    use super::*;
    pub type PlatRegisterFn = unsafe extern "C" fn(KmodId, *const u8, usize, KPlatProbeFn, usize) -> i32;
    pub type PlatUnregisterFn = unsafe extern "C" fn(KmodId) -> i32;
    pub type DtbBlobFn = unsafe extern "C" fn(*mut usize) -> *const u8;
    pub type BoardMacFn = unsafe extern "C" fn(u32, *mut u8) -> i32;
    pub type SocSteppingFn = unsafe extern "C" fn() -> i32;
    pub type Pi5PinsFn = unsafe extern "C" fn(owner: KmodId, op: u32, arg: u32, out: *mut u64) -> i32;
    pub type SerrorCheckpointFn = unsafe extern "C" fn() -> u64;
    pub type VmapFn = unsafe extern "C" fn(*const u64, usize, u32) -> usize;
    pub type VunmapFn = unsafe extern "C" fn(usize);
    pub type DmaQuiesceRegisterFn = unsafe extern "C" fn(KmodId, *const u8, usize, u32, KDmaQuiesceFn, usize) -> i32;
    extern "C" {
        /// `compats`: NUL-separated compatible strings. 0 ok, -22 bad args.
        pub fn kapi_plat_register(owner: KmodId, compats: *const u8, len: usize, probe: KPlatProbeFn, ctx: usize) -> i32;
        pub fn kapi_plat_unregister(owner: KmodId) -> i32;
        /// Recorded DTB; length through `len`. Null when the board has none.
        pub fn kapi_dtb_blob(len: *mut usize) -> *const u8;
        /// Factory MAC (6 bytes) into `out`. -19 on boards without one.
        pub fn kapi_board_mac(which: u32, out: *mut u8) -> i32;
        /// BCM2712 stepping: 0 C0, 1 D0, 2 unknown. -19 on other boards.
        pub fn kapi_soc_stepping() -> i32;
        /// Pi 5 Wi-Fi pins, for the module named `brcmf` only (-1 otherwise): `pi5_pins::MUX_SDIO2` (out[0] stepping 0 C0 / 1 D0,
        /// out[1] packed fsel nibbles of gpio30..35 before, out[2] register writes; -19 unknown stepping or off the Pi 5, -5 mux
        /// failure), `WL_ON_SET` (arg 0/1), `WL_ON_READY` (0, or -19 when the gio block is unavailable).
        pub fn kapi_pi5_pins(owner: KmodId, op: u32, arg: u32, out: *mut u64) -> i32;
        /// Pending SError syndrome since the last checkpoint, 0 when clean (always 0 off aarch64).
        pub fn kapi_serror_checkpoint() -> u64;
        /// Map `n` physical pages (array of page addresses) contiguously in the kernel window. `cache`: 0 UC, 1 WC, 2 WB. 0 on failure.
        pub fn kapi_vmap(pages: *const u64, n: usize, cache: u32) -> usize;
        pub fn kapi_vunmap(va: usize);
        pub fn kapi_dma_quiesce_register(owner: KmodId, name: *const u8, len: usize, stage: u32, f: KDmaQuiesceFn, ctx: usize) -> i32;
    }
}
