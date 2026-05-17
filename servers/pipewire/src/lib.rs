//! PipeWire server for LeandrOS.
//!
//! Provides a minimal PipeWire-compatible IPC interface over AF_UNIX sockets.

#![no_std]

use ipc::{Message, port};
use spin::Mutex;
use drivers::snd::VirtioSnd;
use drivers::Driver;

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

/// Initialize PipeWire server
pub fn init() -> Result<u32, i32> {
    let mut state = STATE.lock();
    
    // 1. Initialize sound driver
    if let Err(_) = state.snd_driver.probe() {
        return Err(-19); // ENODEV
    }

    // 2. Register IPC port
    let server_port = port::create(0).ok_or(-12)?; // ENOMEM
    if !port::register_handler(server_port, handle_msg) {
        return Err(-1);
    }
    state.bound_port = server_port;

    // 3. Bind AF_UNIX socket (via net_server)
    net_server::force_bind_unix(PW_SOCKET_PATH, server_port);

    state.initialized = true;
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
            
            let mut kbuf = [0u8; 1024]; // Handle in small chunks for simplicity
            let mut total_written = 0;
            
            while total_written < count {
                let chunk_size = (count - total_written).min(kbuf.len());
                let ok = sched::with_current_address_space(|as_| {
                    as_.read_user_buf(buf_ptr + total_written, &mut kbuf[..chunk_size])
                }).unwrap_or(false);
                
                if !ok { return err_reply(-14); } // EFAULT
                
                state.snd_driver.send_pcm_data(&kbuf[..chunk_size]);
                total_written += chunk_size;
            }
            
            val_reply(total_written as u64)
        }
        0x1000 => { // PING
            let mut resp = Message::empty();
            resp.tag = 0x1001; // PONG
            resp
        },
        // Handle PipeWire protocol messages here
        0x100 | 0x200 => { // SET_PARAMS or PCM data
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
