//! DRM server for hardware-accelerated graphics
//!
//! This server implements the DRM (Direct Rendering Manager) interface,
//! providing userspace applications like DOOM access to hardware-accelerated graphics.

#![no_std]

use ipc::{Message, port};
use drivers::drm_device_interface::DrmDeviceInterface;
use drivers::Driver;
use vfs_server;

extern "C" { fn arch_serial_putc(c: u8); }

fn serial_debug(msg: &str) {
    if !drivers::pci::RENDER_DEBUG { return; }
    for &b in msg.as_bytes() {
        unsafe { arch_serial_putc(b); }
    }
}

fn serial_debug_hex(v: u32) {
    if !drivers::pci::RENDER_DEBUG { return; }
    serial_debug("0x");
    for i in (0..8).rev() {
        let n = (v >> (i * 4)) & 0xF;
        let c = if n < 10 { b'0' + n as u8 } else { b'A' + n as u8 - 10 };
        unsafe { arch_serial_putc(c); }
    }
}

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

fn ok_reply()        -> Message { make_reply(0) }
fn err_reply(e: i32) -> Message { make_reply(e as i64) }
fn val_reply(v: u64) -> Message { make_reply(v as i64) }

/// Global DRM interface instance (initialized on first use)
static mut INTERFACE: Option<DrmDeviceInterface> = None;

/// Handle DRM device requests
fn handle(msg: &Message, _caller_pid: u32, _target_port: u32) -> Message {
    let interface = unsafe {
        if INTERFACE.is_none() {
            let mut i = DrmDeviceInterface::new();
            if i.probe().is_ok() {
                INTERFACE = Some(i);
            } else {
                return err_reply(-19); // ENODEV
            }
        }
        INTERFACE.as_mut().unwrap()
    };

    // Handle VFS ioctl messages
    if msg.tag == 0x28 { // VFS_IOCTL
        let cmd = arg(msg, 1) as u32;
        let arg_val = arg(msg, 2) as usize;

        serial_debug("[DRM-SRV] handle VFS_IOCTL cmd=");
        serial_debug_hex(cmd);
        serial_debug("\n");

        // Since we are called via a direct handler, we are in the caller's address space.
        // No need to switch address space.
        let res = match interface.handle_ioctl(cmd, arg_val) {
            Ok(result) => val_reply(result as u64),
            Err(_) => err_reply(-1),
        };

        serial_debug("[DRM-SRV] handle_ioctl finished, returning\n");
        return res;
    }
 else if msg.tag == 0x12 { // VFS_WRITE
        let count = arg(msg, 2) as usize;
        let buf_ptr = arg(msg, 1) as usize;

        if buf_ptr == 0 { return err_reply(-14); } // EFAULT
        let buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, count) };

        match interface.handle_write(buf) {
            Ok(n) => val_reply(n as u64),
            Err(_) => err_reply(-5), // EIO
        }
    } else if msg.tag == vfs_server::VFS_READ {
        // read() on the card fd returns queued drm_event_vblank blobs (page-flip
        // completions). Drain into a kernel-local buffer, then copy into the
        // caller's address space via the safe path (NOT a raw deref — the read
        // path is not guaranteed to run in the caller's AS). EAGAIN when empty.
        let buf_ptr = arg(msg, 1) as usize;
        let count = arg(msg, 2) as usize;
        let pid = arg(msg, 3) as u32;
        if buf_ptr == 0 { return err_reply(-14); } // EFAULT

        let mut kbuf = [0u8; 256];
        let cap = count.min(kbuf.len());
        let n = drivers::drm_device_interface::drm_read_events(&mut kbuf[..cap]);
        if n == 0 { return err_reply(-11); } // EAGAIN

        let ok = sched::with_task_address_space(pid, || {
            unsafe {
                core::ptr::copy_nonoverlapping(kbuf.as_ptr(), buf_ptr as *mut u8, n);
            }
            0i32
        });
        match ok {
            Some(0) => val_reply(n as u64),
            _ => err_reply(-14), // EFAULT
        }
    } else if msg.tag == vfs_server::VFS_POLL {
        // POLLIN when a page-flip event is queued to read.
        let revents: u32 = if drivers::drm_device_interface::drm_has_events() { 0x1 } else { 0 };
        // (revents, seq): seq echoes the delivered-flip counter so epoll's
        // edge emulation re-arms on each new event.
        let seq = drivers::drm_device_interface::drm_event_seq();
        let mut m = Message::empty();
        m.data[0..8].copy_from_slice(&(revents as u64).to_le_bytes());
        m.data[8..16].copy_from_slice(&(seq as u64).to_le_bytes());
        m
    } else if msg.tag == vfs_server::VFS_CLOSE || msg.tag == vfs_server::VFS_CLOSE_ALL {
        interface.release();
        ok_reply()
    } else {
        interface.handle(msg.clone())
    }
}

/// Initialize DRM service
pub fn init(owner_pid: u32) -> Option<u32> {
    let port_id = port::create(owner_pid)?;
    vfs_server::register_device("/dev/dri/card0", port_id, 0, 226, 0);
    port::register_handler(port_id, handle);
    // Throttled page-flip event delivery runs off the 100 Hz tick. This does NOT
    // displace the audio pump (register_tick_hook fills the next free slot).
    sched::register_tick_hook(drivers::drm_device_interface::drm_tick);
    Some(port_id)
}
