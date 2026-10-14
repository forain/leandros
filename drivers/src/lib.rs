//! Driver framework — drivers are user-space servers communicating via IPC.
//!
//! This crate provides the skeleton each driver server implements.
//! Mirrors Linux's driver model (bus/device/driver) but enforced by the
//! microkernel: a crashing driver doesn't take down the kernel.

#![no_std]

extern crate alloc;

pub mod serial;
pub mod pci;
/// Runtime IRQ registration, x86_64 vector allocator, MSI/MSI-X (lane kfacil).
pub mod irq;
/// `request_firmware`: kernel read of a file from the mounted root (lane kfacil).
pub mod firmware;
pub mod nvidia_probe;
pub mod snd;
/// Intel High Definition Audio controller + codec playback (lane hda).
pub mod hda;
pub mod framebuffer;
pub mod kms;
pub mod vector_font;
pub mod glyphs;
pub mod drm;
pub mod drm_driver;
pub mod drm_console;
pub mod console_commands;
pub mod console_properties;
pub mod drm_device_interface;
/// Pure DRM sync-object state (binary + timeline); host-tested in drivers/uapi-tests.
pub mod syncobj_core;
/// Seam for a GPU driver in another crate answering a driver-private DRM UAPI
/// (the NVIDIA nouveau-UAPI layer in drivers/nvidia).
pub mod gpu_uapi;
pub mod gpu_kms;
/// Sync files and dma-buf implicit fences (zink on a GPU UAPI backend).
pub mod sync_file;
/// Pure sync-file / dma-buf reservation semantics; host-tested in drivers/uapi-tests.
pub mod sync_file_core;
pub mod virtio;
pub mod virtio_gpu;
pub mod virtio_blk;
pub mod virtio_keyboard;
pub mod virtio_net;
/// Realtek RTL8111/8168 PCIe gigabit Ethernet (real PCs).
pub mod r8169;
/// Hardware-independent r8169 pieces (host-tested in drivers/r8169-tests).
pub mod r8169_hw;
/// The NIC behind `eth0`: virtio-net or r8169.
pub mod netdev;
pub mod virtio_rng;
pub mod usb_hcd;
pub mod usb_hid;
/// USB mass storage (xHCI bulk-only + SCSI) registered as `sdX` block devices.
pub mod usb_storage;
#[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
pub mod sdhci;
/// VideoCore mailbox — the only source of a framebuffer on a Raspberry Pi 5,
/// whose firmware publishes no `simple-framebuffer` DTB node. Compiled for
/// `raspi4b` too, where QEMU models the same property interface and the whole
/// protocol can be exercised without hardware.
#[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
pub mod rpi_mailbox;

/// Block-device trait and types every backend implements (always built).
pub mod blockdev;
/// Pure MBR/GPT parser (host-tested in drivers/partscan-tests).
pub mod partscan;

/// The block layer callers use (kernel/src/init.rs, kernel/src/syscall.rs,
/// servers/f2fs). On QEMU `virt`/x86_64 it is the runtime registry
/// (`blkdev.rs`): virtio disks at boot, USB disks via `blkdev::register` at
/// any time, partitions scanned from both. On rpi5/raspi4b it stays the
/// compile-time SD-card path (`blkdev_sdhci.rs` over `sdhci`; no PCI bus
/// exists on either target). Both expose the same functions.
#[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
#[path = "blkdev_sdhci.rs"]
pub mod blkdev;
#[cfg(not(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b"))))]
pub mod blkdev;

/// Per-device metadata surfaced to userspace via the block-device-enumeration
/// syscalls (see kernel/src/syscall.rs SYS_BLKDEV_INFO), backing `lsblk`.
#[derive(Debug, Clone, Copy)]
pub struct BlkDevInfo {
    pub total_blocks: u64,
    pub block_size: u32,
    pub fstype: Option<&'static str>,
}

/// Trait every driver server must implement.
pub trait Driver {
    /// One-time hardware initialisation.
    fn probe(&mut self) -> Result<(), DriverError>;
    /// Called when the driver's IPC port receives a message.
    fn handle(&mut self, msg: ipc::Message) -> ipc::Message;
}

#[derive(Debug, Clone, Copy)]
pub enum DriverError {
    NotFound,
    Io,
    Unsupported,
    InvalidParameter,
    /// The caller is not permitted to drive this device right now — it does not
    /// hold DRM master, or the VT it holds it on is not the one on screen.
    ///
    /// Answered to userspace as **EACCES**, which is exactly what Linux's
    /// `drm_ioctl_permit()` returns when a `DRM_MASTER` ioctl arrives from a
    /// non-master. The errno is load-bearing, not decorative: smithay maps
    /// EACCES to `DrmError::Access` and logs it, whereas ENODEV makes it tear
    /// the whole device down and takes the compositor with it.
    Access,
    /// Another open already holds DRM master. **EBUSY**, as Linux's
    /// `drm_setmaster_ioctl` answers for the same case.
    Busy,
    /// `DROP_MASTER` from an open that never held it. **EINVAL**, as Linux's
    /// `drm_dropmaster_ioctl` answers.
    NotMaster,
    /// An exact errno (positive, e.g. 19 for ENODEV), for a driver UAPI whose
    /// callers branch on the value (the nouveau UAPI: NVK maps ENODEV to
    /// VK_ERROR_DEVICE_LOST). The DRM server answers `-errno`.
    Errno(i32),
}
