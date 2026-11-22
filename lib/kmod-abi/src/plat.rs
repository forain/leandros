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

/// HDMI sink facts from the boot probe (`kapi_hdmi_sink_info`). `size` is `size_of::<KHdmiSink>()`.
/// `state`: hdmi_probe PROBE_* (2 = done). `port_flags`: bit0 valid, 1 hpd, 2 hdmi mode, 3 active, 4 gate open (hdmi0).
/// `pix_khz`: measured pixel clock (0 unknown). `sink`: EDID verdict, 0 unknown, 1 HDMI, 2 DVI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KHdmiSink { pub size: u32, pub state: u32, pub port_flags: u32, pub pix_khz: u32, pub sink: u32, pub d0: u32 }

pub mod sys_dma40 {
    use super::KHdmiSink;
    pub type Dma40ClaimFn = unsafe extern "C" fn(i32) -> i64;
    pub type Dma40ReleaseFn = unsafe extern "C" fn(u32) -> i32;
    pub type Dma40StartCyclicFn = unsafe extern "C" fn(u32, u64, usize, usize, u64, u32, u32) -> i64;
    pub type Dma40StopFn = unsafe extern "C" fn(u32) -> i32;
    pub type Dma40PositionFn = unsafe extern "C" fn(u32) -> i64;
    pub type Dma40PeriodsFn = unsafe extern "C" fn(u32) -> u64;
    pub type Dma40PollFn = unsafe extern "C" fn(u32) -> i32;
    pub type Dma40PolledFn = unsafe extern "C" fn() -> i32;
    pub type HdmiSinkInfoFn = unsafe extern "C" fn(*mut KHdmiSink) -> i32;
    extern "C" {
        /// Claim a dma40 channel (`prefer` < 0 = any). Handle (> 0) or -errno (-19 not a BCM2712, -16 none free).
        pub fn kapi_dma40_claim(prefer: i32) -> i64;
        pub fn kapi_dma40_release(h: u32) -> i32;
        /// Cyclic mem-to-FIFO ring (CPU physical addresses, `dreq_cell` = the DT `dmas` cell). Periods or -errno.
        pub fn kapi_dma40_start_cyclic(h: u32, ring: u64, len: usize, period: usize, fifo: u64, dreq_cell: u32, irq_per_period: u32) -> i64;
        /// 0, -110 when the abort timed out, -22 bad handle.
        pub fn kapi_dma40_stop(h: u32) -> i32;
        /// Byte offset the engine reads next, -1 when no ring runs, -22 bad handle.
        pub fn kapi_dma40_position(h: u32) -> i64;
        pub fn kapi_dma40_periods(h: u32) -> u64;
        /// Service a pending INT (polled mode): 1 serviced, 0 none, -22 bad handle.
        pub fn kapi_dma40_poll(h: u32) -> i32;
        /// 1 when the engine completes by polling (no IRQ), 0 IRQ mode, -19 off a BCM2712.
        pub fn kapi_dma40_polled() -> i32;
        /// 0 ok, -22 null/short buffer.
        pub fn kapi_hdmi_sink_info(out: *mut KHdmiSink) -> i32;
    }
}

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
    pub type Rp1IrqRequestFn = unsafe extern "C" fn(KmodId, u32, bool, crate::KIrqHandler, usize) -> i32;
    pub type Rp1IrqFreeFn = unsafe extern "C" fn(KmodId, u32) -> i32;
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
        /// Attach `h(ctx)` to RP1 interrupt source `n` (level = re-arm after each run). 0, -16 busy, -19 no RP1, -22, -95.
        pub fn kapi_rp1_irq_request(owner: KmodId, n: u32, level: bool, h: crate::KIrqHandler, ctx: usize) -> i32;
        pub fn kapi_rp1_irq_free(owner: KmodId, n: u32) -> i32;
    }
}

/// V3D (Pi 5) core services for the `v3d` module; every call returns -19 (or 0/false) off a Pi 5 or when V3D=n.
pub mod sys_v3d {
    use crate::{KIrqHandler, KmodId};
    pub type BoHoldFn = unsafe extern "C" fn(u32, u32) -> i64;
    pub type BoUnrefFn = unsafe extern "C" fn(u32);
    pub type BoFenceFn = unsafe extern "C" fn(u32, u32, u64);
    pub type QueueWaitFn = unsafe extern "C" fn(u64) -> i32;
    pub type FenceAllocFn = unsafe extern "C" fn() -> u64;
    pub type FenceDoneFn = unsafe extern "C" fn(u64) -> i32;
    pub type FenceRetireFn = unsafe extern "C" fn(u64);
    pub type VaAllocFn = unsafe extern "C" fn(u64) -> u64;
    pub type VaFreeFn = unsafe extern "C" fn(u64);
    pub type RegisterFn = unsafe extern "C" fn(KmodId, *const u8, usize) -> i32;
    pub type ArmFn = unsafe extern "C" fn(*const u8, usize) -> i32;
    pub type MboxPropertyFn = unsafe extern "C" fn(*const u32, usize, *mut u32, usize) -> i64;
    pub type PmBaseFn = unsafe extern "C" fn() -> u64;
    pub type IrqRequestSpiFn = unsafe extern "C" fn(KmodId, u32, KIrqHandler, usize) -> i64;
    pub type IrqLineStateFn = unsafe extern "C" fn(u32) -> u32;
    extern "C" {
        /// > 0 BO object held, 0 dumb BO (no reference), -2 not reachable by this open.
        pub fn kapi_v3d_bo_hold(handle: u32, open_id: u32) -> i64;
        pub fn kapi_v3d_bo_unref(obj: u32);
        pub fn kapi_v3d_bo_fence(handle: u32, open_id: u32, fence: u64);
        /// Park the submitter up to 1 ms for queue room. 1 = a signal is pending (give up), 0 = retry.
        pub fn kapi_v3d_queue_wait(deadline: u64) -> i32;
        pub fn kapi_v3d_fence_alloc() -> u64;
        pub fn kapi_v3d_fence_done(fence: u64) -> i32;
        /// Wake sync-object waiters of `fence` (any context).
        pub fn kapi_v3d_fence_retire(fence: u64);
        /// Kernel-owned GPU VA span; 0 = none.
        pub fn kapi_v3d_va_alloc(bytes: u64) -> u64;
        pub fn kapi_v3d_va_free(va: u64);
        /// Hand over the `KV3dOps` table (permanent). -22 bad size, -17 already set, -19 no V3D.
        pub fn kapi_v3d_register(owner: KmodId, ops: *const u8, size: usize) -> i32;
        /// Latch the backend armed and pick the scanout sink. -22 with no table.
        pub fn kapi_v3d_arm(cmdline: *const u8, len: usize) -> i32;
        /// One firmware property call, serialised with the kernel's own. Reply words copied out; count or -errno.
        pub fn kapi_mbox_property(words: *const u32, n: usize, out: *mut u32, out_n: usize) -> i64;
        /// Shared PM window virtual base, 0 if unmapped.
        pub fn kapi_pm_base() -> u64;
        /// GIC SPI `intid` (not PCI INTx). Line or -errno.
        pub fn kapi_irq_request_spi(owner: KmodId, intid: u32, handler: KIrqHandler, ctx: usize) -> i64;
        pub fn kapi_irq_line_state(id: u32) -> u32;
    }
}
