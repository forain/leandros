//! USB hub driver — ported from drivers/usb/core/hub.c
//!
//! Handles device enumeration, port reset, speed detection, and address
//! assignment for devices downstream of a USB hub.

use crate::descriptor::{
    HubDescriptor, UsbSpeed,
    PORT_STATUS_CONNECTION,
    PORT_STATUS_LOW_SPEED, PORT_STATUS_HIGH_SPEED,
    PORT_CHANGE_CONNECTION, PORT_CHANGE_RESET,
};
use crate::device::{UsbDevice, DeviceState};
use crate::hcd::HostControllerDriver;

/// Maximum USB device address (§11.23.1).
pub const USB_MAX_ADDRESS: u8 = 127;

/// Back-to-back PORTSC reads/RW1C-writes with no gap between them were
/// observed (via QEMU's `qemu-xhci` model) to corrupt unrelated kernel
/// memory or hang the guest — each operation alone is safe, but the
/// sequence isn't, unless given a moment to settle. No real per-port
/// debounce timing is implemented here either (a real hub driver would
/// wait ~100ms after connect before reset); this is a minimal spin delay,
/// not a hardware-accurate wait.
fn settle() {
    for _ in 0..100_000 {
        core::hint::spin_loop();
    }
}

/// Per-port state tracked by the hub driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortState {
    /// No device connected.
    Empty,
    /// Connection detected, waiting for debounce.
    Debouncing,
    /// In reset sequence.
    Resetting,
    /// Enumerating the new device.
    Enumerating,
    /// Device fully configured.
    Active,
    /// Port disabled or error.
    Disabled,
}

/// Hub driver instance — one per hub device.
pub struct HubDriver {
    pub desc:    HubDescriptor,
    pub nports:  u8,
    pub port_state: [PortState; 16], // up to 15 downstream ports
    next_address: u8,
}

impl HubDriver {
    pub fn new(desc: HubDescriptor) -> Self {
        Self {
            nports: desc.b_nbr_ports,
            desc,
            port_state: [PortState::Empty; 16],
            next_address: 1,
        }
    }

    // ── Port power ────────────────────────────────────────────────────────────

    /// Power on all ports (mirrors hub_power_on in hub.c).
    pub fn power_on_ports<H: HostControllerDriver>(&self, hcd: &mut H) {
        for port in 1..=self.nports {
            hcd.set_port_feature(port, HUB_PORT_FEAT_POWER);
            // In real Linux: wait b_pwr_on_2_pwr_good × 2 ms.
        }
    }

    // ── Port status polling ───────────────────────────────────────────────────

    /// Handle a port status change event (called when hub reports status change
    /// on interrupt IN endpoint).  Returns new device if one was enumerated.
    ///
    /// Mirrors hub_port_connect_change / hub_event in hub.c.
    pub fn handle_port_change<H: HostControllerDriver>(
        &mut self,
        hcd: &mut H,
        port: u8,
    ) -> Option<UsbDevice> {
        let (status, change) = hcd.get_port_status(port)?;
        settle();

        // Acknowledge change bits.
        if change & PORT_CHANGE_CONNECTION != 0 {
            hcd.clear_port_feature(port, HUB_PORT_FEAT_C_CONNECTION);
            settle();
        }
        if change & PORT_CHANGE_RESET != 0 {
            hcd.clear_port_feature(port, HUB_PORT_FEAT_C_RESET);
            settle();
        }

        if status & PORT_STATUS_CONNECTION == 0 {
            // Device was removed.
            self.port_state[port as usize] = PortState::Empty;
            return None;
        }

        // New connection — start enumeration.
        self.port_state[port as usize] = PortState::Enumerating;
        self.enumerate_device(hcd, port, status)
    }

    // ── Enumeration ───────────────────────────────────────────────────────────

    fn enumerate_device<H: HostControllerDriver>(
        &mut self,
        hcd: &mut H,
        port: u8,
        port_status: u16,
    ) -> Option<UsbDevice> {
        // 1. Determine speed (mirrors usb_detect_quirks / hub_port_init).
        let speed = Self::port_speed(port_status);

        // 2. Issue port reset (≥10 ms).
        hcd.port_reset(port);
        settle();

        // 3. Create device at address 0, read first 8 bytes of DeviceDescriptor.
        let mut dev = UsbDevice::new(0, speed);
        dev.state = DeviceState::Default;

        let dd = hcd.get_device_descriptor(&dev)?;
        dev.device_desc = dd;

        // 4. Assign address.
        let addr = self.alloc_address()?;
        hcd.set_address(&mut dev, addr);
        dev.devnum = addr;
        dev.state = DeviceState::Address;

        // 5. Read full DeviceDescriptor now that address is set.
        let full_dd = hcd.get_device_descriptor(&dev)?;
        dev.device_desc = full_dd;

        // 6. Read and set configuration 1.
        let cd = hcd.get_config_descriptor(&dev, 0)?;
        hcd.set_configuration(&mut dev, cd.b_configuration_value);
        dev.active_config = Some(cd);
        dev.state = DeviceState::Configured;

        self.port_state[port as usize] = PortState::Active;
        Some(dev)
    }

    fn port_speed(status: u16) -> UsbSpeed {
        if status & PORT_STATUS_LOW_SPEED != 0 {
            UsbSpeed::Low
        } else if status & PORT_STATUS_HIGH_SPEED != 0 {
            UsbSpeed::High
        } else {
            UsbSpeed::Full
        }
    }

    fn alloc_address(&mut self) -> Option<u8> {
        if self.next_address > USB_MAX_ADDRESS { return None; }
        let a = self.next_address;
        self.next_address += 1;
        Some(a)
    }
}

// Hub class-specific port feature selectors (ch11.h C_PORT_* / PORT_*).
pub const HUB_PORT_FEAT_CONNECTION:   u16 = 0;
pub const HUB_PORT_FEAT_ENABLE:       u16 = 1;
pub const HUB_PORT_FEAT_SUSPEND:      u16 = 2;
pub const HUB_PORT_FEAT_OVERCURRENT:  u16 = 3;
pub const HUB_PORT_FEAT_RESET:        u16 = 4;
pub const HUB_PORT_FEAT_L1:           u16 = 5;
pub const HUB_PORT_FEAT_POWER:        u16 = 8;
pub const HUB_PORT_FEAT_LOWSPEED:     u16 = 9;
pub const HUB_PORT_FEAT_C_CONNECTION: u16 = 16;
pub const HUB_PORT_FEAT_C_ENABLE:     u16 = 17;
pub const HUB_PORT_FEAT_C_SUSPEND:    u16 = 18;
pub const HUB_PORT_FEAT_C_OVERCURRENT: u16 = 19;
pub const HUB_PORT_FEAT_C_RESET:      u16 = 20;
pub const HUB_PORT_FEAT_C_L1:         u16 = 23;
