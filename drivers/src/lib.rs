//! Driver framework — drivers are user-space servers communicating via IPC.
//!
//! This crate provides the skeleton each driver server implements.
//! Mirrors Linux's driver model (bus/device/driver) but enforced by the
//! microkernel: a crashing driver doesn't take down the kernel.

#![no_std]

extern crate alloc;

pub mod serial;
pub mod kenv;
pub mod platform;
pub mod pci;
/// Generic PCI config-space save/restore for system sleep (lane s3pci).
pub mod pci_pm;
/// Runtime IRQ registration, x86_64 vector allocator, MSI/MSI-X (lane kfacil).
pub mod irq;
/// `request_firmware`: kernel read of a file from the mounted root (lane kfacil).
pub mod firmware;
pub mod nvidia_probe;
/// Laptop device inventory and read-only probes (ACPI ids, PCI BARs, TPM, Thunderbolt, SMBus accelerometer).
#[cfg(all(target_arch = "x86_64", feature = "drv-lapinv"))]
pub mod lapinv;
/// CONFIG_LAPINV=n: the boot inventory does not run.
#[cfg(all(target_arch = "x86_64", not(feature = "drv-lapinv")))]
pub mod lapinv {
    pub fn run(_rsdp_phys: u64, _hhdm: u64, _cmdline: &str) {}
}
/// Intel Gen9 graphics: probe, GT bring-up, self-tests (lane i915).
#[cfg(feature = "i915")]
pub mod i915;
/// `i915` off: same API, nothing probed (CONFIG_I915=n).
#[cfg(not(feature = "i915"))]
#[path = "i915_stub.rs"]
pub mod i915;
pub mod acpi_power;
/// HID report-descriptor parser (pure logic, host-tested; lane lapi2c).
pub mod hid_desc;
pub mod hid_ptp;
/// ACPI I2C slave discovery, DesignWare I2C master, HID-over-I2C probe (lane lapi2c).
#[cfg(all(target_arch = "x86_64", feature = "drv-i2c-hid"))]
pub mod i2c_acpi;
#[cfg(all(target_arch = "x86_64", feature = "drv-i2c-hid"))]
pub mod dw_i2c;
#[cfg(all(target_arch = "x86_64", feature = "drv-i2c-hid"))]
pub mod i2c_hid;
/// CONFIG_I2C_HID=n: no I2C touchpad; the PS/2 path stays as is.
#[cfg(all(target_arch = "x86_64", not(feature = "drv-i2c-hid")))]
pub mod i2c_hid {
    pub fn set_ps2_reinit(_f: fn()) {}
    pub fn poll_interval_ns() -> u64 { 1_000_000_000 }
    pub fn init(_cmdline: &str) -> bool { false }
    pub fn listen_once(_now_ns: u64) -> bool { false }
}
pub mod power_sysfs;
#[cfg(feature = "virtio-snd")]
pub mod snd;
/// Intel High Definition Audio controller + codec playback (lane hda).
#[cfg(feature = "drv-hda")]
pub mod hda;
/// CONFIG_HDA=n: no HD Audio controller; `probe` always fails.
#[cfg(not(feature = "drv-hda"))]
pub mod hda {
    use alloc::string::String;
    pub const TX_BUF_BYTES: usize = crate::snd::TX_BUF_BYTES;
    pub struct Hda;
    impl Hda {
        pub const fn new() -> Self { Hda }
        pub fn probe(&mut self) -> Result<(), ()> { Err(()) }
        pub fn reconfigure_stream(&mut self, _stream_id: u32, _freq: u32, _channels: u8) {}
        pub fn recover_stream(&mut self) {}
        pub fn queued_bytes(&self) -> u64 { 0 }
        pub fn tx_level(&self) -> u16 { 0 }
        pub fn tx_used_idx(&self) -> u16 { 0 }
        pub fn top_up_silence(&mut self) {}
        pub fn send_pcm_data(&mut self, _data: &[u8]) -> usize { 0 }
        pub fn shutdown(&mut self, _link_reset: bool, _budget_us: u64) -> String { String::new() }
        pub fn pci_function(&self) -> Option<crate::pci::PciFunction> { None }
        pub fn info(&mut self) -> String { String::new() }
        pub fn capture_ensure(&mut self) -> Result<(), i32> { Err(-19) }
        pub fn capture_restart(&mut self) -> bool { false }
        pub fn capture_pending(&self) -> usize { 0 }
        pub fn capture_read(&mut self, _out: &mut [u8]) -> Result<usize, i32> { Err(-19) }
        pub fn jack_service(&mut self) {}
        pub fn capture_close(&mut self) {}
    }
}
pub mod framebuffer;
pub mod kms;
pub mod kms_v3d;
pub mod vector_font;
pub mod glyphs;
pub mod drm;
pub mod drm_driver;
pub mod drm_console;
pub mod console_commands;
pub mod console_properties;
pub mod drm_device_interface;
pub mod device_mmap_policy;
pub mod drm_offset;
/// Pure DRM sync-object state (binary + timeline); host-tested in drivers/uapi-tests.
pub mod syncobj_core;
pub mod hostvis_core;
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
#[cfg(feature = "drv-virtio-blk")]
pub mod virtio_blk;
pub mod virtio_keyboard;
#[cfg(feature = "virtio-net")] pub mod virtio_net;
/// Realtek RTL8111/8168 PCIe gigabit Ethernet (real PCs).
#[cfg(feature = "r8169")] pub mod r8169;
/// Hardware-independent r8169 pieces (host-tested in drivers/r8169-tests).
#[cfg(feature = "r8169")] pub mod r8169_hw;
/// Intel e1000e family (I219 PCH NICs, QEMU's 82574L).
#[cfg(feature = "e1000e")] pub mod e1000e;
/// Hardware-independent e1000e pieces (host-tested in drivers/e1000e-tests).
#[cfg(feature = "e1000e")] pub mod e1000e_hw;
/// Cadence GEM (macb) in the Raspberry Pi 5's RP1, BCM54213PE PHY; inert until the RP1 probe attaches it.
#[cfg(feature = "macb")] pub mod macb;
pub mod dma_shutdown;
/// Hardware-independent macb pieces (host-tested in drivers/macb-tests).
#[cfg(feature = "macb")] pub mod macb_hw;
/// The NIC behind `eth0`: virtio-net, r8169 or e1000e.
pub mod netdev;
pub mod netshape;
/// Intel Wireless 8265/8275 (iwlwifi, MVM firmware) and its pieces: PCIe
/// transport, firmware file parser, firmware command codec, 802.11 frames,
/// WPA2 crypto (the last four host-tested in drivers/iwlwifi-tests).
#[cfg(feature = "iwlwifi")] pub mod iwlwifi;
pub mod cmdline_secrets;
#[cfg(feature = "iwlwifi")] pub mod iwl_trans;
#[cfg(feature = "iwlwifi")] pub mod iwl_fw;
#[cfg(feature = "wifi-stack")] pub mod iwl_cmd;
#[cfg(feature = "iwlwifi")] pub mod iwl_assoc;
#[cfg(feature = "iwlwifi")] pub mod iwl_rs;
#[cfg(feature = "wifi-stack")] pub mod iwl_agg;
#[cfg(feature = "wifi-stack")] pub mod wifi_frame;
/// WMM, HT and Block Ack (incl. the RX reorder buffer), shared by iwlwifi
/// and wifi_sim (host-tested in drivers/iwlwifi-tests).
#[cfg(feature = "wifi-stack")] pub mod wifi_ht;
#[cfg(feature = "wifi-stack")] pub mod wifi_crypto;
/// Wrong-passphrase block policy of the manager (host-tested in drivers/brcmf-tests).
#[cfg(feature = "wifi-stack")] pub mod wifi_wrongkey;
/// The control seam between /dev/netctl and Wi-Fi backends (types only so far).
pub mod wifi_api;
/// The Wi-Fi connection manager behind that seam (iwlwifi.mgr=1, wifi_sim).
#[cfg(feature = "wifi-stack")] pub mod wifi_mgr;
/// The Wi-Fi radio class: `Radio` trait + `repr(C)` table (pure) and the kernel registry.
#[cfg(feature = "wifi-stack")] pub mod wifi_radio;
#[cfg(feature = "wifi-stack")] pub mod wifi_class;
/// Broadcom/Cypress FullMAC Wi-Fi (CYW43455, Raspberry Pi 5).
#[cfg(feature = "brcmf")] pub mod brcmf;
pub mod bcm2712_id;
/// BCM2712 GPIO (gio/gio_aon), pin mux (C0/D0) and l2 interrupt controllers.
pub mod bcm2712_gpio;
/// Pi 5 board serial and factory MACs from the OTP (firmware mailbox).
pub mod bcm2712_otp;
/// Pi 5 read-only HDMI0/1 boot probe (HDMI vs DVI, pixel clock, audio gate, MAI); rpi5 only.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod hdmi_probe;
pub mod hdmi_audio;
/// Pi 5 Wi-Fi boot bring-up: sdio2 host → firmware → "ver" (rpi5 only).
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
#[cfg(feature = "brcmf")] pub mod brcmf_boot;
/// Bluetooth: H4 transport + Broadcom bring-up (CYW43455 BT half on the Pi 5 uarta).
pub mod bt;
#[cfg(feature = "wifi-stack")] pub mod wifi_pref;
/// A simulated Wi-Fi interface for QEMU tests (opt-in; see its header).
#[cfg(feature = "wifi-sim")] pub mod wifi_sim;
#[cfg(feature = "drv-virtio-rng")]
pub mod virtio_rng;
/// CONFIG_VIRTIO_RNG=n: no hardware entropy device.
#[cfg(not(feature = "drv-virtio-rng"))]
pub mod virtio_rng {
    pub const MAX_READ: usize = 4096;
    pub fn read(_out: &mut [u8]) -> usize { 0 }
    pub fn init() {}
}
pub mod usb_hcd;
pub mod usb_hid;
pub mod rel_pointer;
pub mod psmouse;
/// USB mass storage (xHCI bulk-only + SCSI) registered as `sdX` block devices.
pub mod usb_storage;
/// NVMe SSDs registered as `nvme<C>n<N>` block devices (read-only unless `nvme.rw=1`).
#[cfg(feature = "drv-nvme")]
pub mod nvme;
/// CONFIG_NVME=n or =m: the entry points the kernel calls, inert (a module registers its own pm hooks).
#[cfg(not(feature = "drv-nvme"))]
#[path = "stubs/nvme.rs"]
pub mod nvme;
#[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
pub mod sdhci;
/// VideoCore mailbox — the only source of a framebuffer on a Raspberry Pi 5,
/// whose firmware publishes no `simple-framebuffer` DTB node. Compiled for
/// `raspi4b` too, where QEMU models the same property interface and the whole
/// protocol can be exercised without hardware.
#[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
pub mod rpi_mailbox;
/// Pi 5 SoC housekeeping: RNG, firmware RTC, PM watchdog, thermal, ARM clock (lane pimisc).
pub mod rpi_misc_logic;
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod rpi_misc;
/// Shared BCM2712 PM block accessor (V3D power + watchdog/reset).
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod rpi_pm;
/// SoC temperature API (`thermal::soc_millicelsius`) and /proc hooks.
pub mod thermal;
/// BCM2712 PCIe root complex (pcie2) register math, RC bring-up, and the RP1
/// south bridge behind it. On by default; `pcie.rpi5=0` turns it off.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod brcmstb_hw;
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod brcmstb_pcie;
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod rp1;
/// Pi 5 USB: RP1's two DWC3 controllers in xHCI host mode (glue + attach).
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod rp1_usb;
/// DWC3 global-register math (host-tested in drivers/dwc3-tests).
pub mod dwc3_hw;
/// RP1 clock/GPIO/PWM register math (host-tested in drivers/rp1-tests).
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod rp1_hw;
/// RP1 clock manager: PLL readback, GEM and PWM clock enables.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod rp1_clk;
/// RP1 GPIO/pinctrl and its small kernel API.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod rp1_gpio;
/// Pi 5 cooling fan on RP1 PWM1.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod rp1_fan;
/// Pi 5 ACT/PWR LEDs and the power button (KEY_POWER).
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod pi5_board;
/// BCM2712 dma40 DMA engine (Pi 5): SCB encoding and rings everywhere, the
/// channel driver and boot self-test rpi5-only.
pub mod bcm2712_dma40;
/// Broadcom V3D 7.1 GPU (Pi 5): register map, power-up, MMU. Pure logic
/// everywhere; the hardware probe is rpi5-only.
pub mod v3d;

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

// Stubs for the Kconfig-gated drivers that are switched off (see stubs/).
#[cfg(not(feature = "virtio-snd"))]
#[path = "stubs/snd.rs"]
pub mod snd;
#[cfg(not(feature = "virtio-net"))]
#[path = "stubs/virtio_net.rs"]
pub mod virtio_net;
#[cfg(not(feature = "r8169"))]
#[path = "stubs/r8169.rs"]
pub mod r8169;
#[cfg(not(feature = "e1000e"))]
#[path = "stubs/e1000e.rs"]
pub mod e1000e;
#[cfg(not(feature = "iwlwifi"))]
#[path = "stubs/iwlwifi.rs"]
pub mod iwlwifi;
#[cfg(not(feature = "wifi-stack"))]
#[path = "stubs/wifi_mgr.rs"]
pub mod wifi_mgr;
