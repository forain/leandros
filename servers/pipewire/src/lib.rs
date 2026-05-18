//! PipeWire server for LeandrOS.
//!
//! Provides a minimal PipeWire-compatible IPC interface over AF_UNIX sockets.

#![no_std]

use ipc::{Message, port};
use spin::Mutex;
use drivers::snd::VirtioSnd;
use drivers::Driver;
use drivers::pci;

pub const PW_SOCKET_PATH: &str = "/run/pipewire/pipewire-0";

struct PipeWireState {
    snd_driver: VirtioSnd,
    bound_port: u32,
    initialized: bool,
}

impl PipeWireState {
    const fn new() -> Self {
        Self {
            snd_driver: VirtioSnd::new(),
            bound_port: 0,
            initialized: false,
        }
    }
}

static STATE: Mutex<PipeWireState> = Mutex::new(PipeWireState::new());

// ── Protocol helper ──────────────────────────────────────────────────────────

fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off+8].try_into().unwrap())
}

fn make_reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&v.to_le_bytes());
    m
}

fn val_reply(v: u64) -> Message { make_reply(v as i64) }
fn err_reply(e: i32) -> Message { make_reply(e as i64) }

pub fn init() -> Result<u32, i32> {
    let mut state = STATE.lock();
    
    pci::serial_debug("[PW] Initializing...\n");
    // 1. Initialize sound driver
    if let Err(_) = state.snd_driver.probe() {
        pci::serial_debug("[PW] Sound driver probe failed\n");
        return Err(-19); // ENODEV
    }

    // 2. Register IPC port
    let server_port = port::create(0).ok_or(-12)?; // ENOMEM
    if !port::register_handler(server_port, handle_msg) {
        return Err(-1);
    }
    state.bound_port = server_port;

    // 3. Register with VFS
    vfs_server::register_device("/dev/pipewire", server_port, 0);

    // 4. Bind AF_UNIX socket (via net_server)
    net_server::force_bind_unix(PW_SOCKET_PATH, server_port);

    state.initialized = true;
    pci::serial_debug("[PW] Ready on port ");
    pci::serial_debug_hex(server_port);
    pci::serial_debug("\n");
    Ok(server_port)
}

fn handle_msg(msg: &Message, _caller_pid: u32) -> Message {
    handle(msg)
}

/// Main IPC handler for PipeWire server
pub fn handle(msg: &Message) -> Message {
    let mut state = STATE.lock();
    if !state.initialized {
        return Message::empty();
    }

    match msg.tag {
        0x12 => { // VFS_WRITE
            let buf_ptr = arg(msg, 1) as usize;
            let count = arg(msg, 2) as usize;
            if count == 0 { return val_reply(0); }
            
            // Safety: Validate pointer or handle in chunks
            // For now, we trust the kernel-side caller (VFS proxy)
            let mut kbuf = [0u8; 512];
            let mut total = 0;
            while total < count {
                let n = (count - total).min(512);
                let ok = sched::with_current_address_space(|as_| {
                    as_.read_user_buf(buf_ptr + total, &mut kbuf[..n])
                }).unwrap_or(false);
                if !ok { return err_reply(-14); }
                state.snd_driver.send_pcm_data(&kbuf[..n]);
                total += n;
            }
            val_reply(total as u64)
        }
        0x28 => { // VFS_IOCTL
            let cmd = arg(msg, 1) as u32;
            let arg_val = arg(msg, 2) as usize;
            if cmd == 0x101 { // SET_PARAMS
                // Read from user-space arg_val
                let mut params = [0u8; 8];
                let ok = sched::with_current_address_space(|as_| {
                    as_.read_user_buf(arg_val, &mut params)
                }).unwrap_or(false);
                if !ok { return err_reply(-14); }
                let freq = u32::from_le_bytes([params[0], params[1], params[2], params[3]]);
                let channels = params[4];
                state.snd_driver.reconfigure_stream(0, freq, channels);
                return val_reply(0);
            }
            err_reply(-25) // ENOTTY
        }
        0x1000 => { // PING
            let mut resp = Message::empty();
            resp.tag = 0x1001; // PONG
            resp
        },
        0x100 | 0x200 => { // Direct SET_PARAMS or PCM data
            state.snd_driver.handle(msg.clone())
        },
        _ => Message::empty()
    }
}

/// Register PipeWire as a system service.
pub fn register_service() -> u32 {
    let state = STATE.lock();
    if state.bound_port == 0 {
        drop(state);
        if let Ok(p) = init() {
            return p;
        }
    } else {
        return state.bound_port;
    }
    0
}
