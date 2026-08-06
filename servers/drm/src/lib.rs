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

// ── Device nodes ─────────────────────────────────────────────────────────────
//
// One port, one `DrmDeviceInterface`, one GPU — two nodes. `dev_id` (ioctl slot
// 0) says which node a request arrived on; it is orthogonal to `open_id`
// (slot 4), which says which *open* of that node it arrived on and is what the
// per-open virtgpu 3D context is keyed by. Two opens of renderD128 get two
// open_ids and therefore two contexts, exactly as two opens of card0 do.
const DEV_ID_CARD: u32 = 0;
const DEV_ID_RENDER: u32 = 1;

/// Linux's DRM char major, and the two minors. Render minors start at 128.
const DRM_MAJOR: u32 = 226;
const CARD_MINOR: u32 = 0;
const RENDER_MINOR: u32 = 128;

const DRM_IOCTL_VERSION: u32 = 0xC0406400;

/// Not a Linux ioctl number: the kernel's `sys_mmap` sends this to a
/// DynamicDevice to resolve an mmap offset to a guest-physical token
/// (kernel/src/syscall.rs, `sys_mmap`).
const DRM_IOCTL_MMAP: u32 = 0x1007;

/// Slot-1 mapping hint in a 0x1007 reply: "do not map this range write-back".
/// Mirrored in kernel/src/syscall.rs — the two must agree.
const MMAP_HINT_UNCACHED: u64 = 1;

/// `struct drm_version` — mirrored here (it is also defined privately in
/// `drivers::drm_device_interface`) because the render node has to answer
/// DRM_IOCTL_VERSION with a *different* identity than card0 does, and that
/// decision belongs to the node, not to the shared device interface.
///
/// 64 bytes: three i32 then 4 bytes of padding before the first `usize`, which
/// is what the 0x0040 size field of the ioctl request code encodes.
#[repr(C)]
struct drm_version {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: u64,
    date_len: usize,
    date: u64,
    desc_len: usize,
    desc: u64,
}

/// DRM_IOCTL_VERSION on the RENDER node.
///
/// Mesa's Venus ICD refuses any render node whose driver is not literally
/// `virtio_gpu` at major version 0 (`vn_renderer_virtgpu.c`, virtgpu_open_device:
/// `strcmp(version->name, "virtio_gpu") || version->version_major != 0`), so the
/// render node reports the upstream virtio-gpu identity — which is the truth
/// about it: everything reachable through it is the virtgpu 3D command stream.
///
/// card0 deliberately keeps reporting `leandros-drm` 1.6.0. Mesa's DRI loader
/// picks a driver .so by exactly this string (`loader_get_driver_for_fd`), and
/// card0 is the fd the whole COSMIC/GBM/softpipe path runs on: naming it
/// `virtio_gpu` would send that loader looking for `virtio_gpu_dri.so` instead
/// of falling through to the software backend it uses today. Per-node identities
/// keep that path untouched.
///
/// Runs in the caller's address space (direct port handler), same as the
/// card0 version handler it shadows.
fn render_node_version(arg: usize) -> Message {
    if arg == 0 { return err_reply(-22); } // EINVAL
    let v = unsafe { &mut *(arg as *mut drm_version) };
    // Upstream virtio_gpu's DRIVER_MAJOR/MINOR/PATCHLEVEL.
    v.version_major = 0;
    v.version_minor = 1;
    v.version_patchlevel = 0;

    let name = "virtio_gpu\0";
    let date = "0\0";
    let desc = "virtio GPU\0";

    // Two-pass contract: the caller first asks with null pointers to learn the
    // lengths, then again with buffers. Always report the lengths; only fill a
    // buffer that exists and is big enough.
    if v.name != 0 && v.name_len >= name.len() {
        unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), v.name as *mut u8, name.len()); }
    }
    v.name_len = name.len();
    if v.date != 0 && v.date_len >= date.len() {
        unsafe { core::ptr::copy_nonoverlapping(date.as_ptr(), v.date as *mut u8, date.len()); }
    }
    v.date_len = date.len();
    if v.desc != 0 && v.desc_len >= desc.len() {
        unsafe { core::ptr::copy_nonoverlapping(desc.as_ptr(), v.desc as *mut u8, desc.len()); }
    }
    v.desc_len = desc.len();
    ok_reply()
}

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
        let dev_id = arg(msg, 0) as u32;
        let cmd = arg(msg, 1) as u32;
        let arg_val = arg(msg, 2) as usize;
        // Slot 4 is the VFS's per-open cookie (see VnodeKind::DynamicDevice).
        // It has to be a parameter rather than state on INTERFACE: port
        // handlers run synchronously on the calling thread, so this handler is
        // re-entrant across clients.
        let open_id = arg(msg, 4) as u32;

        serial_debug("[DRM-SRV] handle VFS_IOCTL cmd=");
        serial_debug_hex(cmd);
        serial_debug("\n");

        // The only ioctl whose answer depends on WHICH node it arrived on.
        // Everything else — including every virtgpu 3D ioctl — is identical on
        // both nodes and keeps routing purely by open_id.
        if dev_id == DEV_ID_RENDER && cmd == DRM_IOCTL_VERSION {
            return render_node_version(arg_val);
        }

        // Since we are called via a direct handler, we are in the caller's address space.
        // No need to switch address space.
        let res = match interface.handle_ioctl(cmd, arg_val, open_id) {
            Ok(result) => {
                let mut m = val_reply(result as u64);
                // Slot 0 is the physical token. Slot 1 tells `sys_mmap` how to
                // map it: a host-visible virtio-gpu blob the host asked to be WC
                // or UNCACHED must NOT come out write-back, or the guest reads
                // its own stale cache lines instead of the host's writes.
                // Everything else — dumb buffers, guest-backed blobs, the
                // framebuffer — answers CACHED and leaves the hint at zero.
                //
                // Asked here rather than inside `handle_ioctl_mmap` so the
                // lookup happens with no BO lock held.
                if cmd == DRM_IOCTL_MMAP {
                    let cache = drivers::drm_device_interface::blob_map_cache_type(result as u64);
                    let uncached = cache == drivers::virtio_gpu::VIRTIO_GPU_MAP_CACHE_UNCACHED
                        || cache == drivers::virtio_gpu::VIRTIO_GPU_MAP_CACHE_WC;
                    if uncached {
                        m.data[8..16].copy_from_slice(&MMAP_HINT_UNCACHED.to_le_bytes());
                    }
                    // The scoping is the point, so it is traced either way: one
                    // line per resolved mmap token saying which cache type the
                    // host asked for and which mapping it therefore gets. A
                    // Venus session must show `map_info=0x01 -> writeback` for
                    // the command ring and `map_info=0x03 -> uncached` for
                    // Mesa's fence-feedback buffer.
                    // Unconditional: `serial_debug` here is gated on
                    // RENDER_DEBUG, and this line is the only evidence that the
                    // scoping is by cache type rather than blanket.
                    drivers::pci::serial_debug("[DRM-SRV] mmap token=");
                    drivers::pci::serial_debug_hex_64(result as u64);
                    drivers::pci::serial_debug(" map_info=0x0");
                    drivers::pci::serial_debug(match cache {
                        0 => "0", 1 => "1", 2 => "2", 3 => "3", _ => "?",
                    });
                    drivers::pci::serial_debug(
                        if uncached { " -> uncached\n" } else { " -> writeback\n" });
                }
                m
            }
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
    } else if msg.tag == vfs_server::VFS_CLOSE {
        // The VFS sends this once the LAST fd on this open is gone, with the
        // node in slot 0 and the open cookie in slot 1. Retire whatever that
        // open owned (its virtgpu 3D context) before the generic release.
        drivers::drm_device_interface::drm_release_open(arg(msg, 1) as u32);
        // `release()` re-enables the framebuffer console, which is only ever
        // right for the node that could have taken it away. A render node has
        // no KMS relationship at all, so closing one MUST NOT resurrect the
        // console underneath a compositor still scanning out on card0.
        if arg(msg, 0) as u32 == DEV_ID_CARD {
            interface.release();
        }
        ok_reply()
    } else if msg.tag == vfs_server::VFS_CLOSE_ALL {
        // Dead code today — the VFS retires fds one at a time through
        // release_vnode, so card0 never sees this. No open identity to key on.
        interface.release();
        ok_reply()
    } else {
        interface.handle(msg.clone())
    }
}

/// Initialize DRM service
pub fn init(owner_pid: u32) -> Option<u32> {
    let port_id = port::create(owner_pid)?;
    vfs_server::register_device("/dev/dri/card0", port_id, DEV_ID_CARD, DRM_MAJOR, CARD_MINOR);
    // The render node. Same port, same DrmDeviceInterface, same GPU — a second
    // dev_id is all it takes, exactly as evdev registers event0/event1 (see
    // servers/evdev/src/lib.rs:426). It exists because Mesa's Venus ICD never
    // looks at card0: virtgpu_open() enumerates with drmGetDevices2() and
    // requires `available_nodes & (1 << DRM_NODE_RENDER)`, then opens
    // `nodes[DRM_NODE_RENDER]`. The matching sysfs attributes that let
    // drmGetDevices2() classify the device at all are built into the image by
    // scripts/mkfs-f2fs-populated.py.
    vfs_server::register_device("/dev/dri/renderD128", port_id, DEV_ID_RENDER, DRM_MAJOR, RENDER_MINOR);
    port::register_handler(port_id, handle);
    // Teach the VFS how to drop the DRM reference an exported dmabuf fd holds.
    //
    // This crate is the one place that already depends on BOTH `vfs_server` and
    // `drivers`, so the edge lives here and `vfs-server` keeps its dependency
    // list unchanged — it must not gain `drivers`. Registering here also means
    // a build with no DRM device never registers at all, and the VFS's null
    // check makes that a no-op, which is correct: nothing can have exported.
    //
    // The callee is invoked from tmpfs inode teardown with every VFS guard
    // already dropped (see `DMABUF_RELEASE` in servers/vfs), because it takes
    // VIRTIO_GPU and busy-spins on a device round-trip.
    vfs_server::set_dmabuf_release(drivers::drm_device_interface::bo_release_exported);
    // Throttled page-flip event delivery runs off the 100 Hz tick. This does NOT
    // displace the audio pump (register_tick_hook fills the next free slot).
    sched::register_tick_hook(drivers::drm_device_interface::drm_tick);
    Some(port_id)
}
