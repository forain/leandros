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
        0x1000 => { // PING
            let mut resp = Message::empty();
            resp.tag = 0x1001; // PONG
            resp
        },
        // Handle PipeWire protocol messages here
        0x200 => { // PCM data
            // We can't use pci::serial_debug here easily without adding it to the crate.
            // But we can use the net_server::force_bind_unix logic or just assume 
            // the driver logging will show it.
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
