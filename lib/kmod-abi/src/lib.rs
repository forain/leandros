//! Frozen kmod ABI v1. See docs/design (kmod L3b). Any change bumps
//! `KMOD_ABI_VERSION`.
#![cfg_attr(not(test), no_std)]

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
    pub const TICK: RegKind = RegKind(8);
    pub const PM: RegKind = RegKind(9);
    pub const SND: RegKind = RegKind(10);
    pub const BLK: RegKind = RegKind(11);
    pub const INPUT: RegKind = RegKind(12);
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
/// Strongly-ordered uncached device mapping in kernel VA space (what `ioremap(.., Uncached)` gives);
/// the right mode for register BARs. Honoured together with KMAP_WRITABLE.
pub const KMAP_UC: u32 = 8;

/// `kapi_netdev_register` kinds.
pub const KNET_WIRED: u32 = 0;
pub const KNET_WIFI: u32 = 1;

/// Network-interface driver operations (module side of the kernel's `NetifOps`). Every entry takes
/// the `ctx` given at registration first. `link` fills (up, Mbit/s, full duplex); `mac` writes 6 bytes
/// and returns false when unknown; `recv` returns the frame length or a negative value for none;
/// `info` and `driver_name` copy up to `cap` bytes into `buf` and return the length.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KNetifOps {
    pub send: extern "C" fn(ctx: usize, buf: *const u8, len: usize) -> bool,
    pub tx_ready: extern "C" fn(ctx: usize) -> bool,
    pub recv: extern "C" fn(ctx: usize, buf: *mut u8, cap: usize) -> i32,
    pub rx_rearm: extern "C" fn(ctx: usize) -> bool,
    pub rx_irq_driven: extern "C" fn(ctx: usize) -> bool,
    pub mac: extern "C" fn(ctx: usize, out6: *mut u8) -> bool,
    pub link: extern "C" fn(ctx: usize, up: *mut bool, mbps: *mut u32, full: *mut bool),
    pub link_gen: extern "C" fn(ctx: usize) -> u32,
    pub info: extern "C" fn(ctx: usize, buf: *mut u8, cap: usize) -> usize,
    pub driver_name: extern "C" fn(ctx: usize, buf: *mut u8, cap: usize) -> usize,
}

/// `KSndOps::flags`: the capture entries and `jack_service` are live (HD Audio); the kernel then
/// runs its `[audio]` service thread for the device.
pub const KSND_CAPTURE: u32 = 1;

/// Sound-backend operations (module side of the pipewire server's `Backend`). Every entry takes
/// the `ctx` given at registration first. `write` returns the bytes accepted; `queued_bytes` the
/// bytes in flight; `tx_used_idx` a counter of retired TX chunks (only its changes matter);
/// `cap_read` returns the byte count or a negative errno; `info` and `shutdown` copy up to `cap`
/// bytes of text into `buf` and return the length (`shutdown`: `link_reset` != 0 also holds the link in reset).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KSndOps {
    pub flags: u32,
    pub configure: extern "C" fn(ctx: usize, stream: u32, freq: u32, channels: u8),
    pub write: extern "C" fn(ctx: usize, buf: *const u8, len: usize) -> usize,
    pub queued_bytes: extern "C" fn(ctx: usize) -> usize,
    pub tx_used_idx: extern "C" fn(ctx: usize) -> u32,
    pub top_up_silence: extern "C" fn(ctx: usize),
    pub recover: extern "C" fn(ctx: usize),
    pub cap_ensure: extern "C" fn(ctx: usize) -> i32,
    pub cap_restart: extern "C" fn(ctx: usize),
    pub cap_pending: extern "C" fn(ctx: usize) -> usize,
    pub cap_read: extern "C" fn(ctx: usize, buf: *mut u8, cap: usize) -> i32,
    pub cap_close: extern "C" fn(ctx: usize),
    pub jack_service: extern "C" fn(ctx: usize),
    pub info: extern "C" fn(ctx: usize, buf: *mut u8, cap: usize) -> usize,
    pub shutdown: extern "C" fn(ctx: usize, link_reset: u32, buf: *mut u8, cap: usize) -> usize,
}

/// Power-management hook: writes a message into `buf` (<= `cap`). Return >= 0 = ok with that
/// message length (shutdown: 0 = nothing to log); < 0 = error with message length `-r`.
pub type KPmFn = extern "C" fn(ctx: usize, buf: *mut u8, cap: usize) -> i32;

/// Power-management phases (`KPmOps::phase`). The kernel orders hooks by phase for each operation
/// (suspend: INPUT, GPU, NET, WIFI, BLOCK, USB, PCI; resume is the reverse).
pub mod pm_phase {
    pub const INPUT: u32 = 1;
    pub const GPU: u32 = 2;
    pub const SOUND: u32 = 3;
    pub const NET: u32 = 4;
    pub const WIFI: u32 = 5;
    pub const BLOCK: u32 = 6;
    pub const USB: u32 = 7;
    pub const PCI: u32 = 8;
    pub const CONSOLE: u32 = 9;
    pub const MAX: u32 = 9;
}

/// Hooks for `kapi_pm_register2`. `size` is `size_of::<KPmOps>()` so fields can be appended.
/// Each hook returns >= 0 (ok) or < 0 (error); the message length is |return|, text in `buf`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KPmOps {
    pub size: u32,
    pub phase: u32,
    pub suspend: Option<KPmFn>,
    pub resume: Option<KPmFn>,
    pub freeze: Option<KPmFn>,
    pub thaw: Option<KPmFn>,
    pub shutdown: Option<KPmFn>,
    pub kexec_quiesce: Option<KPmFn>,
    pub resume_check: Option<KPmFn>,
}
/// Interrupt handler: returns 1 when handled, 0 when the interrupt was not the device's.
pub type KIrqHandler = extern "C" fn(ctx: usize) -> u32;


// ── video (L7a): the camera ops table behind /dev/video0, and the USB class-driver seam ──

pub const KFMT_OTHER: u8 = 0;
pub const KFMT_MJPEG: u8 = 1;
pub const KFMT_YUYV: u8 = 2;
pub const KFMT_NV12: u8 = 3;

/// Camera summary: UVC version (bcdUVC, 0 if not UVC), the USB slot (255 = none) and the format count.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct KCamInfo {
    pub bcd: u16,
    pub slot: u8,
    pub nformats: u8,
}

/// One pixel format. `kind` is a `KFMT_*`; `nframes` frame sizes follow via `KCameraOps::frame`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct KFormat {
    pub index: u8,
    pub kind: u8,
    pub bpp: u8,
    pub default_frame: u8,
    pub nframes: u32,
}

/// One frame size; `interval` is in 100 ns units.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct KFrame {
    pub index: u8,
    pub _pad: u8,
    pub width: u16,
    pub height: u16,
    pub _pad2: u16,
    pub max_size: u32,
    pub interval: u32,
}

/// A capture buffer handed to `stream_start` (kernel virtual address).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KCamBuf {
    pub ptr: *mut u8,
    pub cap: usize,
}

/// A completed buffer from `stream_dqbuf`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct KCamDone {
    pub index: u32,
    pub seq: u32,
    pub bytes: u64,
    pub ts_ns: u64,
    pub error: u32,
    pub _pad: u32,
}

/// `stream_poll` result bits.
pub const KCAM_NEW_FRAME: u32 = 1;
pub const KCAM_GONE: u32 = 2;

/// A video-capture device as seen by the kernel's V4L2 layer. Every op takes the driver's `ctx`
/// first. `stream_poll` and `stream_ready`/`stream_gone` may run in interrupt context (the
/// tick), must never block, and must tolerate losing a lock race (return 0). Everything else
/// runs in task context. Errors are `-errno`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KCameraOps {
    pub info: extern "C" fn(ctx: usize, out: *mut KCamInfo) -> i32,
    pub format: extern "C" fn(ctx: usize, index: u32, out: *mut KFormat) -> i32,
    pub frame: extern "C" fn(ctx: usize, format_index: u32, frame_index: u32, out: *mut KFrame) -> i32,
    pub stream_start: extern "C" fn(ctx: usize, format: u8, frame: u8, interval: u32,
                                    bufs: *const KCamBuf, nbufs: u32) -> i32,
    pub stream_stop: extern "C" fn(ctx: usize),
    pub stream_poll: extern "C" fn(ctx: usize) -> u32,
    pub stream_ready: extern "C" fn(ctx: usize) -> u32,
    pub stream_gone: extern "C" fn(ctx: usize) -> bool,
    pub stream_qbuf: extern "C" fn(ctx: usize, index: u32) -> i32,
    /// 0 = a buffer was written to `out`, -11 none ready, -19 the device is gone.
    pub stream_dqbuf: extern "C" fn(ctx: usize, out: *mut KCamDone) -> i32,
    /// Get (`set` false) or set a V4L2 control by CID; the value is read from `val`, and the
    /// resulting value written to `out`. -22 for an unknown control.
    pub control: extern "C" fn(ctx: usize, cid: u32, set: bool, val: u32, out: *mut u32) -> i32,
}

/// A USB interface offered to a class driver. `handle` names the device function for the
/// control/alt/isochronous calls added in L7b. `class`/`subclass`/`protocol` are those of the
/// matched interface.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KUsbIf {
    pub handle: u64,
    pub vid: u16,
    pub pid: u16,
    pub slot: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub iface: u8,
    pub _pad: [u8; 7],
}

/// Class-driver probe: 0 = claimed, non-zero = not mine (the kernel keeps the interface for later drivers).
pub type KUsbProbeFn = extern "C" fn(intf: *const KUsbIf, ctx: usize) -> i32;
/// The device behind `handle` left the bus. The kernel drops the handle (and closes any isochronous
/// pipe still open on it) right after this returns. Task context.
pub type KUsbDetachFn = extern "C" fn(handle: u64, ctx: usize);

/// USB control request header, as on the wire. Bit 7 of `request_type` gives the data direction.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KUsbSetup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

/// Tick callback (USB timer tick, interrupt context).
pub type KTickFn = extern "C" fn(ctx: usize);

/// Every id-table entry starts with a `u16 bus` discriminator.
pub const BUS_PCI: u16 = 1;
pub const BUS_USB: u16 = 2;
pub const BUS_OF: u16 = 3;
pub const BUS_ACPI: u16 = 4;
/// SDIO function-1 CIS manfid (a=vendor, b=device, 0xffff = any). Added by W2-2.
pub const BUS_SDIO: u16 = 5;

pub mod alias;
pub mod blk;
pub mod input;
pub use blk::{KBlkDesc, KBlkOps};

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

    pub type PciEnableFn = unsafe extern "C" fn(u32, bool) -> i32;
    pub type PciFindCapFn = unsafe extern "C" fn(u32, u8) -> u16;
    pub type IrqRequestMsiFn = unsafe extern "C" fn(KmodId, u32, super::KIrqHandler, usize) -> i64;
    pub type IrqRequestLineFn = unsafe extern "C" fn(KmodId, u32, super::KIrqHandler, usize) -> i64;
    pub type WaitNewFn = unsafe extern "C" fn() -> usize;
    pub type WaitTimeoutFn = unsafe extern "C" fn(usize, u64) -> bool;
    pub type WaitNotifyFn = unsafe extern "C" fn(usize);
    pub type NetdevRegisterFn =
        unsafe extern "C" fn(KmodId, u32, *const u8, usize, *const super::KNetifOps, usize) -> i32;
    pub type NetdevKickFn = unsafe extern "C" fn();
    pub type SndRegisterFn = unsafe extern "C" fn(KmodId, *const super::KSndOps, usize) -> i32;
    pub type V4l2RegisterFn = unsafe extern "C" fn(KmodId, *const super::KCameraOps, usize) -> i32;
    pub type PmRegister2Fn = unsafe extern "C" fn(KmodId, *const super::KPmOps, usize) -> i32;
    pub type VirtToPhysFn = unsafe extern "C" fn(usize) -> u64;
    pub type CanSleepFn = unsafe extern "C" fn() -> bool;
    pub type SleepUntilFn = unsafe extern "C" fn(u64);
    pub type YieldFn = unsafe extern "C" fn();
    pub type IoremapFn = unsafe extern "C" fn(u64, usize, u32) -> usize;
    pub type IounmapFn = unsafe extern "C" fn(usize, usize);
    pub type DmaSyncFn = unsafe extern "C" fn(usize, usize, u32);
    pub type MutexTryFn = unsafe extern "C" fn(usize) -> bool;
    pub type MutexNewFn = unsafe extern "C" fn(*const u8, usize) -> usize;
    pub type SemNewFn = unsafe extern "C" fn(u32) -> usize;
    pub type HrtimerNewFn = unsafe extern "C" fn(Option<extern "C" fn(usize)>, usize, bool) -> usize;
    pub type HrtimerArmFn = unsafe extern "C" fn(usize, u64);
    pub type RandomFillFn = unsafe extern "C" fn(*mut u8, usize);
    pub type CopyFromUserFn = unsafe extern "C" fn(*mut u8, usize, usize) -> i32;
    pub type CopyToUserFn = unsafe extern "C" fn(usize, *const u8, usize) -> i32;
    pub type PciScanFn = unsafe extern "C" fn(u32, *mut super::KPciDev) -> i32;
    pub type IrqRequestMsixFn = unsafe extern "C" fn(KmodId, u32, u16, super::KIrqHandler, usize) -> i64;
    pub type FwRequestFn = unsafe extern "C" fn(*const u8, usize, usize, *mut usize, *mut *const u8) -> usize;
    pub type SecretPeekFn = unsafe extern "C" fn(KmodId, *const u8, usize, *mut u8, usize) -> isize;
    pub type ExtEventStatsFn = unsafe extern "C" fn(*mut u64) -> i32;
    pub type ProcPrimeFn = unsafe extern "C" fn(*mut u8, usize) -> isize;
    pub type CurrentPidFn = unsafe extern "C" fn() -> u32;
    pub type TickRegisterFn = unsafe extern "C" fn(KmodId, super::KTickFn, usize) -> i32;
    pub type UsbClassRegisterFn = unsafe extern "C" fn(KmodId, *const super::KmodDevId, usize, super::KUsbProbeFn,
                                                       Option<super::KUsbDetachFn>, usize) -> i32;
    pub type UsbCtrlFn = unsafe extern "C" fn(u64, *const super::KUsbSetup, *mut u8, usize, u32) -> i32;
    pub type UsbSetAltFn = unsafe extern "C" fn(u64, u8, u8) -> i32;
    pub type UsbCfgFn = unsafe extern "C" fn(u64, *mut u8, usize) -> i32;
    pub type UsbIsochOpenFn = unsafe extern "C" fn(u64, u8, u8, u8, usize, usize, bool) -> i64;
    pub type UsbIsochSubmitFn = unsafe extern "C" fn(u64) -> i32;
    pub type UsbIsochReapFn = unsafe extern "C" fn(u64, *mut u8, usize) -> i32;
    pub type UsbIsochCloseFn = unsafe extern "C" fn(u64);
    pub type V4l2UnregisterFn = unsafe extern "C" fn(usize);
    pub type PmRegisterFn = unsafe extern "C" fn(KmodId, Option<super::KPmFn>, Option<super::KPmFn>,
                                                 Option<super::KPmFn>, usize) -> i32;

    extern "C" {
        pub fn kapi_pci_enable(bdf: u32, bus_master: bool) -> i32;
        pub fn kapi_pci_find_cap(bdf: u32, cap_id: u8) -> u16;
        pub fn kapi_irq_request_msi(owner: KmodId, bdf: u32, handler: super::KIrqHandler, ctx: usize) -> i64;
        pub fn kapi_irq_request_line(owner: KmodId, bdf: u32, handler: super::KIrqHandler, ctx: usize) -> i64;
        pub fn kapi_wait_new() -> usize;
        pub fn kapi_wait_timeout(h: usize, ns: u64) -> bool;
        pub fn kapi_wait_notify(h: usize);
        pub fn kapi_netdev_register(owner: KmodId, kind: u32, prefix: *const u8, prefix_len: usize,
                                    ops: *const super::KNetifOps, ctx: usize) -> i32;
        pub fn kapi_snd_register(owner: KmodId, ops: *const super::KSndOps, ctx: usize) -> i32;
        pub fn kapi_netdev_kick();
        pub fn kapi_netdev_kick_from_irq();
        pub fn kapi_v4l2_register(owner: KmodId, ops: *const super::KCameraOps, ctx: usize) -> i32;
        pub fn kapi_pm_register2(owner: KmodId, ops: *const super::KPmOps, ctx: usize) -> i32;
        pub fn kapi_virt_to_phys(va: usize) -> u64;
        pub fn kapi_can_sleep() -> bool;
        pub fn kapi_sleep_until(ns: u64);
        pub fn kapi_yield();
        pub fn kapi_irqs_masked() -> bool;
        pub fn kapi_current_pid() -> u32;
        pub fn kapi_ioremap(phys: u64, len: usize, cache: u32) -> usize;
        pub fn kapi_iounmap(va: usize, len: usize);
        pub fn kapi_dma_sync(va: usize, len: usize, dir: u32);
        pub fn kapi_dma_coherent() -> bool;
        pub fn kapi_mutex_new(name: *const u8, len: usize) -> usize;
        pub fn kapi_mutex_lock(h: usize);
        pub fn kapi_mutex_unlock(h: usize);
        pub fn kapi_mutex_trylock(h: usize) -> bool;
        pub fn kapi_sem_new(n: u32) -> usize;
        pub fn kapi_sem_down_timeout(h: usize, ns: u64) -> bool;
        pub fn kapi_sem_up(h: usize);
        pub fn kapi_hrtimer_new(cb: Option<extern "C" fn(usize)>, ctx: usize, irq_mode: bool) -> usize;
        pub fn kapi_hrtimer_arm(h: usize, ns: u64);
        pub fn kapi_hrtimer_cancel(h: usize);
        pub fn kapi_preempt_disable();
        pub fn kapi_preempt_enable();
        pub fn kapi_random_fill(buf: *mut u8, len: usize);
        pub fn kapi_copy_from_user(dst: *mut u8, uaddr: usize, n: usize) -> i32;
        pub fn kapi_copy_to_user(uaddr: usize, src: *const u8, n: usize) -> i32;
        pub fn kapi_pci_scan(index: u32, out: *mut super::KPciDev) -> i32;
        pub fn kapi_irq_request_msix(owner: KmodId, bdf: u32, entry: u16, handler: super::KIrqHandler, ctx: usize) -> i64;
        pub fn kapi_fw_root_ready() -> bool;
        pub fn kapi_fw_request(path: *const u8, len: usize, max: usize, size: *mut usize, data: *mut *const u8) -> usize;
        pub fn kapi_fw_release(h: usize);
        pub fn kapi_secret_peek(owner: KmodId, key: *const u8, klen: usize, buf: *mut u8, cap: usize) -> isize;
        pub fn kapi_ext_event_stats(out: *mut u64) -> i32;
        pub fn kapi_proc_prime(buf: *mut u8, cap: usize) -> isize;
        pub fn kapi_tick_register(owner: KmodId, f: super::KTickFn, ctx: usize) -> i32;
        pub fn kapi_usb_class_register(owner: KmodId, ids: *const super::KmodDevId, nids: usize,
                                       probe: super::KUsbProbeFn, detach: Option<super::KUsbDetachFn>,
                                       ctx: usize) -> i32;
        pub fn kapi_usb_ctrl(handle: u64, setup: *const super::KUsbSetup, buf: *mut u8, len: usize,
                             timeout_ms: u32) -> i32;
        pub fn kapi_usb_set_alt(handle: u64, iface: u8, alt: u8) -> i32;
        pub fn kapi_usb_cfg(handle: u64, buf: *mut u8, len: usize) -> i32;
        pub fn kapi_usb_isoch_open(handle: u64, iface: u8, alt: u8, ep_addr: u8, fifo_bytes: usize,
                                   ntds: usize, framed: bool) -> i64;
        pub fn kapi_usb_isoch_submit(pipe: u64) -> i32;
        pub fn kapi_usb_isoch_reap(pipe: u64, buf: *mut u8, len: usize) -> i32;
        pub fn kapi_usb_isoch_close(pipe: u64);
        pub fn kapi_v4l2_unregister(ctx: usize);
        pub fn kapi_pm_register(owner: KmodId, suspend: Option<super::KPmFn>, resume: Option<super::KPmFn>,
                                shutdown: Option<super::KPmFn>, ctx: usize) -> i32;
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
    assert!(core::mem::size_of::<KmodDevId>() == 16);
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
    assert!(core::mem::size_of::<KPciDev>() == 40);
    assert!(core::mem::size_of::<KCamInfo>() == 4);
    assert!(core::mem::size_of::<KFormat>() == 8);
    assert!(core::mem::size_of::<KFrame>() == 16);
    assert!(core::mem::size_of::<KCamDone>() == 32);
    assert!(core::mem::size_of::<KUsbIf>() == 24);
    assert!(core::mem::size_of::<KUsbSetup>() == 8);
    assert!(layout::TEST_SLOT_OFF == layout::SIZE - 2 * layout::SLOT_ALIGN);
    assert!(layout::TEST_SLOT_OFF == 0x0ff8_0000);
    assert!(layout::x86_64::BASE % layout::SLOT_ALIGN == 0);
    assert!(layout::aarch64::BASE % layout::SLOT_ALIGN == 0);
};
