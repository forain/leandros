//! DRM server for hardware-accelerated graphics
//!
//! This server implements the DRM (Direct Rendering Manager) interface,
//! providing userspace applications like DOOM access to hardware-accelerated graphics.

#![no_std]

use ipc::{Message, port};
use spin::Mutex;
use drivers::drm_device_interface::DrmDeviceInterface;
use drivers::Driver;
use vfs_server;

// ── Protocol helper ──────────────────────────────────────────────────────────

fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off + 8].try_into().unwrap_or([0u8; 8]))
}

fn make_reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

fn ok_reply() -> Message { val_reply(0) }
fn err_reply(e: i32) -> Message { make_reply(e as i64) }
fn val_reply(v: u64) -> Message { make_reply(v as i64) }

// ── DRM Device State ────────────────────────────────────────────────────────

/// DRM device state
struct DrmDevice {
    interface: Option<DrmDeviceInterface>,
    initialized: bool,
}

impl DrmDevice {
    const fn new() -> Self {
        Self {
            interface: None,
            initialized: false,
        }
    }
}

/// Global DRM device (single device for now)
static DRM_DEVICE: Mutex<DrmDevice> = Mutex::new(DrmDevice::new());

/// Handle DRM device requests
fn handle(msg: &Message, _port: u32) -> Message {
    #[allow(unreachable_code)]
    {
    let mut device = DRM_DEVICE.lock();

    // Initialize device on first access
    if !device.initialized {
        let mut interface = DrmDeviceInterface::new();
        match interface.probe() {
            Ok(()) => {
                device.interface = Some(interface);
                device.initialized = true;
            },
            Err(_) => return err_reply(-19), // ENODEV
        }
    }

    // Handle VFS ioctl messages
    if let Some(ref mut interface) = device.interface {
        // Check message type
        if msg.tag == 0x28 { // VFS_IOCTL
            // Decode ioctl parameters from VFS message format
            let dev_id = arg(msg, 0) as u32;
            let cmd = arg(msg, 1) as u32;
            let arg_val = arg(msg, 2) as usize;

            // Handle the ioctl request properly
            match interface.handle_ioctl(cmd, arg_val) {
                Ok(result) => val_reply(result as u64),
                Err(_) => err_reply(-1),
            }
        } else if msg.tag == 0x12 { // VFS_WRITE
            // ... (existing code)
            val_reply(0)
        } else if msg.tag == vfs_server::VFS_CLOSE || msg.tag == vfs_server::VFS_CLOSE_ALL {
            interface.release();
            ok_reply()
        } else {
            // Handle other message types
            interface.handle(msg.clone())
        }
    } else {
        err_reply(-19) // ENODEV
    }
    } // Close unreachable block
}

/// Initialize DRM service
pub fn init(owner_pid: u32) -> Option<u32> {
    // Create port for DRM server
    let port_id = port::create(owner_pid)?;

    // Register the DRM device with VFS
    vfs_server::register_device("/dev/dri/card0", port_id, 0);

    // Register message handler
    port::register_handler(port_id, handle);

    Some(port_id)
}