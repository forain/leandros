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
    
    // Spooling buffer for non-blocking audio
    spool: [u8; 128 * 1024],
    spool_head: usize,
    spool_tail: usize,
    spool_len: usize,
}

impl PipeWireState {
    pub const fn new() -> Self {
        Self {
            snd_driver: VirtioSnd::new(),
            bound_port: 0,
            initialized: false,
            spool: [0u8; 128 * 1024],
            spool_head: 0,
            spool_tail: 0,
            spool_len: 0,
        }
    }

    fn push_spool(&mut self, data: &[u8]) -> usize {
        let mut total = 0;
        for &b in data {
            if self.spool_len >= self.spool.len() { break; }
            self.spool[self.spool_tail] = b;
            self.spool_tail = (self.spool_tail + 1) % self.spool.len();
            self.spool_len += 1;
            total += 1;
        }
        total
    }

    fn drain_spool(&mut self) {
        if !self.initialized { return; }
        
        while self.spool_len > 0 {
            // Read from spool in chunks of up to 512 bytes
            let mut chunk = [0u8; 512];
            let n = self.spool_len.min(512);
            
            // Handle wrapping
            for i in 0..n {
                chunk[i] = self.spool[(self.spool_head + i) % self.spool.len()];
            }
            
            let accepted = self.snd_driver.send_pcm_data(&chunk[..n]);
            if accepted == 0 { break; } // Hardware ring is full
            
            self.spool_head = (self.spool_head + accepted) % self.spool.len();
            self.spool_len -= accepted;
        }
    }
}

static STATE: Mutex<PipeWireState> = Mutex::new(PipeWireState::new());

// ── Protocol helper ──────────────────────────────────────────────────────────

fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off+8].try_into().unwrap())
}

fn make_reply(val: i64) -> Message {
    let mut m = Message::empty();
    m.tag = 0x8000_0000_0000_0000; // VFS reply tag
    m.data[0..8].copy_from_slice(&val.to_le_bytes());
    m
}

fn val_reply(v: u64) -> Message { make_reply(v as i64) }
fn err_reply(e: i32) -> Message { make_reply(e as i64) }

// ── Service Implementation ───────────────────────────────────────────────────

pub fn init() -> Result<u32, i32> {
    let mut state = STATE.lock();
    
    pci::serial_debug("[PW] Initializing...\n");
    if let Err(_) = state.snd_driver.probe() {
        pci::serial_debug("[PW] Sound driver probe failed\n");
        return Err(-19); // ENODEV
    }

    let server_port = port::create(0).ok_or(-12)?; // ENOMEM
    if !port::register_handler(server_port, handle_msg) {
        return Err(-1);
    }
    state.bound_port = server_port;

    vfs_server::register_device("/dev/pipewire", server_port, 0);
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

pub fn handle(msg: &Message) -> Message {
    let mut state = STATE.lock();
    if !state.initialized {
        return Message::empty();
    }

    state.drain_spool();

    let reply = match msg.tag {
        0x12 => { // VFS_WRITE
            let buf_ptr = arg(msg, 1) as usize;
            let count = arg(msg, 2) as usize;
            if count == 0 { val_reply(0) }
            else {
                let mut kbuf = [0u8; 1024];
                let mut total = 0;
                while total < count {
                    let n = (count - total).min(1024);
                    let ok = sched::with_current_address_space(|as_| {
                        as_.read_user_buf(buf_ptr + total, &mut kbuf[..n])
                    }).unwrap_or(false);
                    if !ok { return err_reply(-14); }
                    let pushed = state.push_spool(&kbuf[..n]);
                    total += pushed;
                    if pushed < n { break; } // Spool full
                }
                val_reply(total as u64)
            }
        }
        0x28 => { // VFS_IOCTL
            let cmd = arg(msg, 1) as u32;
            let arg_val = arg(msg, 2) as usize;
            if cmd == 0x101 { // SET_PARAMS
                let mut params = [0u8; 8];
                let ok = sched::with_current_address_space(|as_| {
                    as_.read_user_buf(arg_val, &mut params)
                }).unwrap_or(false);
                if !ok { err_reply(-14) }
                else {
                    let freq = u32::from_le_bytes([params[0], params[1], params[2], params[3]]);
                    let channels = params[4];
                    state.snd_driver.reconfigure_stream(0, freq, channels);
                    val_reply(0)
                }
            } else {
                err_reply(-25) // ENOTTY
            }
        }
        0x100 | 0x200 => { // Legacy PCM/SET_PARAMS
            if msg.tag == 0x100 {
                let freq = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
                state.snd_driver.reconfigure_stream(0, freq, msg.data[4]);
                Message::empty()
            } else {
                let len = u16::from_le_bytes([msg.data[0], msg.data[1]]) as usize;
                state.push_spool(&msg.data[2..2+len]);
                Message::empty()
            }
        }
        0x300 => { // PUMP
            state.drain_spool();
            Message::empty()
        }
        0x1000 => { // PING
            let mut resp = Message::empty();
            resp.tag = 0x1001; // PONG
            resp
        },
        _ => Message::empty()
    };

    state.drain_spool();
    reply
}
