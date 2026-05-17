#![no_std]

use ipc::{Message, port};
use spin::Mutex;

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

fn err_reply(e: i32) -> Message { make_reply(e as i64) }
fn val_reply(v: u64) -> Message { make_reply(v as i64) }

// ── Linux input_event ────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct input_event {
    pub time: timeval,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

// ── Device State ─────────────────────────────────────────────────────────────

const MAX_EVENTS: usize = 64;
const MAX_DEVICES: usize = 4;

struct EvdevDevice {
    events: [input_event; MAX_EVENTS],
    head:   usize,
    tail:   usize,
    count:  usize,
    in_use: bool,
}

impl EvdevDevice {
    const fn empty() -> Self {
        Self {
            events: [const { input_event {
                time: timeval { tv_sec: 0, tv_usec: 0 },
                type_: 0, code: 0, value: 0
            } }; MAX_EVENTS],
            head:   0,
            tail:   0,
            count:  0,
            in_use: false,
        }
    }

    fn push(&mut self, ev: input_event) {
        if self.count >= MAX_EVENTS {
            self.head = (self.head + 1) % MAX_EVENTS;
            self.count -= 1;
        }
        self.events[self.tail] = ev;
        self.tail = (self.tail + 1) % MAX_EVENTS;
        self.count += 1;
    }

    fn pop(&mut self) -> Option<input_event> {
        if self.count == 0 { return None; }
        let ev = self.events[self.head];
        self.head = (self.head + 1) % MAX_EVENTS;
        self.count -= 1;
        Some(ev)
    }
}

static DEVICES: Mutex<[EvdevDevice; MAX_DEVICES]> = Mutex::new([const { EvdevDevice::empty() }; MAX_DEVICES]);

// ── Interrupt Safety ─────────────────────────────────────────────────────────

extern "C" {
    fn arch_interrupt_save() -> usize;
    fn arch_interrupt_restore(f: usize);
}

// ── Message Dispatch ──────────────────────────────────────────────────────────

pub fn handle(msg: &Message, _caller_pid: u32) -> Message {
    let tag = msg.tag;
    let dev_id = arg(msg, 0) as usize;
    
    if dev_id >= MAX_DEVICES { return err_reply(-19); } // ENODEV
    
    match tag {
        vfs_server::VFS_READ => {
            let buf_ptr = arg(msg, 1) as usize;
            let count = arg(msg, 2) as usize;
            
            let f = unsafe { arch_interrupt_save() };
            let mut devs = DEVICES.lock();
            let dev = &mut devs[dev_id];
            
            if dev.count == 0 {
                drop(devs);
                unsafe { arch_interrupt_restore(f); }
                return err_reply(-11); // EAGAIN
            }
            
            let event_size = core::mem::size_of::<input_event>();
            let mut n = 0;
            let mut events_to_copy = [input_event {
                time: timeval { tv_sec: 0, tv_usec: 0 },
                type_: 0, code: 0, value: 0
            }; 8]; // Copy in chunks
            
            let mut total_copied = 0;
            while n + event_size <= count {
                let mut chunk_count = 0;
                while chunk_count < 8 && n + event_size <= count {
                    if let Some(ev) = dev.pop() {
                        events_to_copy[chunk_count] = ev;
                        chunk_count += 1;
                        n += event_size;
                    } else {
                        break;
                    }
                }
                
                if chunk_count > 0 {
                    let bytes = chunk_count * event_size;
                    let ok = sched::with_current_address_space(|as_| {
                        unsafe {
                            as_.write_user_buf(buf_ptr + total_copied, 
                                core::slice::from_raw_parts(&events_to_copy as *const _ as *const u8, bytes))
                        }
                    }).unwrap_or(false);
                    
                    if !ok {
                        drop(devs);
                        unsafe { arch_interrupt_restore(f); }
                        return err_reply(-14); // EFAULT
                    }
                    total_copied += bytes;
                } else {
                    break;
                }
            }
            drop(devs);
            unsafe { arch_interrupt_restore(f); }
            val_reply(total_copied as u64)
        }
        vfs_server::VFS_WRITE => {
            let count = arg(msg, 2) as u64;
            val_reply(count)
        }
        vfs_server::VFS_IOCTL => {
            let cmd = arg(msg, 1) as usize;
            if cmd == 0x541B { // FIONREAD
                let arg_ptr = arg(msg, 2) as usize;
                let f = unsafe { arch_interrupt_save() };
                let devs = DEVICES.lock();
                let count = (devs[dev_id].count * core::mem::size_of::<input_event>()) as i32;
                drop(devs);
                unsafe { arch_interrupt_restore(f); }
                
                let ok = sched::with_current_address_space(|as_| {
                    unsafe {
                        as_.write_user_buf(arg_ptr, core::slice::from_raw_parts(&count as *const _ as *const u8, 4))
                    }
                }).unwrap_or(false);
                
                if !ok { return err_reply(-14); } // EFAULT
                return val_reply(0);
            }
            if cmd == 0x80044501 { // EVIOCGVERSION
                return val_reply(0x00010001);
            }
            if cmd == 0x80084502 { // EVIOCGID
                let arg_ptr = arg(msg, 2) as usize;
                let mut ids = [0u16; 4];
                ids[0] = 0x0001; // bustype (BUS_USB)
                ids[1] = 0x1234; // vendor
                ids[2] = 0x5678; // product
                ids[3] = 0x0001; // version
                
                let ok = sched::with_current_address_space(|as_| {
                    unsafe {
                        as_.write_user_buf(arg_ptr, core::slice::from_raw_parts(ids.as_ptr() as *const u8, 8))
                    }
                }).unwrap_or(false);
                
                if !ok { return err_reply(-14); } // EFAULT
                return val_reply(0);
            }

            // EVIOCGBIT(ev, len) - base is 0x4520 + ev
            let ioctl_base = cmd & 0xFF;
            if (0x20..0x40).contains(&ioctl_base) && (cmd >> 8) & 0xFF == 0x45 {
                let ev_type = ioctl_base - 0x20;
                let arg_ptr = arg(msg, 2) as usize;
                let max_len = (cmd >> 16) & 0x3FFF;

                if ev_type == 0 { // Supported event types (EV_SYN=0, EV_KEY=1)
                    let bits: u32 = 0x03;
                    let n = core::cmp::min(max_len as usize, 4);
                    unsafe { core::ptr::copy_nonoverlapping(&bits as *const u32 as *const u8, arg_ptr as *mut u8, n) };
                    return val_reply(n as u64);
                } else if ev_type == 1 { // EV_KEY - supported keys
                    // Report a generous range of keys as supported for the virtual keyboard
                    let n = core::cmp::min(max_len as usize, 64);
                    unsafe { core::ptr::write_bytes(arg_ptr as *mut u8, 0xFF, n) };
                    return val_reply(n as u64);
                }
                return val_reply(0);
            }
            err_reply(-25) // ENOTTY
        }
        _ => err_reply(-38), // ENOSYS
    }
}

pub fn pop_event(dev_id: u32) -> Option<input_event> {
    if dev_id as usize >= MAX_DEVICES { return None; }
    let f = unsafe { arch_interrupt_save() };
    let mut devs = DEVICES.lock();
    let ev = devs[dev_id as usize].pop();
    drop(devs);
    unsafe { arch_interrupt_restore(f); }
    ev
}

pub fn has_events(dev_id: u32) -> bool {
    if dev_id as usize >= MAX_DEVICES { return false; }
    let f = unsafe { arch_interrupt_save() };
    let devs = DEVICES.lock();
    let count = devs[dev_id as usize].count;
    drop(devs);
    unsafe { arch_interrupt_restore(f); }
    count > 0
}

pub fn push_event(dev_id: u32, type_: u16, code: u16, value: i32) {
    if dev_id as usize >= MAX_DEVICES { return; }
    let now_ticks = sched::ticks();
    let ev = input_event {
        time: timeval {
            tv_sec: (now_ticks / 100) as i64,
            tv_usec: ((now_ticks % 100) * 10000) as i64,
        },
        type_,
        code,
        value,
    };
    let f = unsafe { arch_interrupt_save() };
    let mut devs = DEVICES.lock();
    devs[dev_id as usize].push(ev);
    drop(devs);
    unsafe { arch_interrupt_restore(f); }
}

pub fn init(owner_pid: u32) -> Option<u32> {
    let port_id = port::create(owner_pid)?;
    {
        let mut devs = DEVICES.lock();
        devs[0].in_use = true; // event0 (keyboard)
    }
    vfs_server::register_device("/dev/input/event0", port_id, 0);
    port::register_handler(port_id, handle);
    Some(port_id)
}
