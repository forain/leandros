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

// A tablet emits X + Y + SYN (and buttons) per motion frame; a fast drag bursts
// well past 64, so the ring is 256 to avoid dropping frames mid-gesture.
const MAX_EVENTS: usize = 256;
const MAX_DEVICES: usize = 4;

// Device classes we surface. Keyboard = event0 (13:64), tablet = event1 (13:65,
// absolute pointer: ABS_X/ABS_Y + BTN_LEFT, NO INPUT_PROP_DIRECT so libinput
// classifies it as a pointer, not a touchscreen).
const DEV_KEYBOARD: usize = 0;
const DEV_TABLET: usize = 1;

struct EvdevDevice {
    events: [input_event; MAX_EVENTS],
    head:   usize,
    tail:   usize,
    count:  usize,
    in_use: bool,
    /// CLOCK id events are stamped with (EVIOCSCLOCKID). 1 = CLOCK_MONOTONIC,
    /// which is what libinput requests; we always stamp from the same monotonic
    /// clock `clock_gettime(CLOCK_MONOTONIC)` reports, so this is advisory only
    /// (kept for a truthful EVIOCGCLOCKID-style answer and future REALTIME
    /// support).
    clockid: u32,
    /// Monotonic push counter — the poll/epoll readiness sequence (edge emu).
    seq: u64,
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
            clockid: 1, // CLOCK_MONOTONIC
            seq: 0,
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
        self.seq = self.seq.wrapping_add(1);
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
    /// Monotonic nanoseconds since boot, sub-tick resolution, never decreasing —
    /// the same clock `clock_gettime(CLOCK_MONOTONIC)` reports to userspace.
    fn arch_monotonic_ns() -> u64;
}

/// Total events ever handed to `push_event`, across every device. A guest-side
/// witness that host-injected input actually reached the kernel ring: QMP
/// accepting `input-send-event` only proves the host queued it.
static EVENTS_PUSHED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// See `EVENTS_PUSHED`.
pub fn events_pushed() -> u64 {
    EVENTS_PUSHED.load(core::sync::atomic::Ordering::Relaxed)
}

// ── evdev capability constants (linux/input-event-codes.h) ────────────────────

const BUS_VIRTUAL: u16 = 0x06;
// EV_SYN=0, EV_KEY=1, EV_ABS=3. Type bitmask: kbd = SYN|KEY = 0x03,
// tablet = SYN|KEY|ABS = 0x0B.
const ABS_X: usize = 0;
const ABS_Y: usize = 1;

// ── User-copy helpers (all go through the caller's address space) ─────────────

fn copy_out(pid: u32, dst: usize, src: &[u8]) -> Message {
    let n = src.len();
    let srcp = src.as_ptr() as usize;
    let ok = sched::with_task_address_space(pid, || {
        unsafe { core::ptr::copy_nonoverlapping(srcp as *const u8, dst as *mut u8, n); }
        0i32
    });
    match ok { Some(0) => val_reply(n as u64), _ => err_reply(-14) }
}

fn copy_in(pid: u32, src: usize, dst: &mut [u8]) -> Option<()> {
    let n = dst.len();
    let dstp = dst.as_mut_ptr() as usize;
    sched::with_task_address_space(pid, || {
        unsafe { core::ptr::copy_nonoverlapping(src as *const u8, dstp as *mut u8, n); }
        0i32
    }).map(|_| ())
}

fn zero_out(pid: u32, dst: usize, len: usize) -> Message {
    let ok = sched::with_task_address_space(pid, || {
        unsafe { core::ptr::write_bytes(dst as *mut u8, 0, len); }
        0i32
    });
    match ok { Some(0) => val_reply(len as u64), _ => err_reply(-14) }
}

/// EVIOCGBIT(ev, len): report the capability bitmask for event type `ev`.
fn eviocgbit(dev_id: usize, ev: usize, arg_ptr: usize, size: usize, pid: u32) -> Message {
    const MAXB: usize = 96; // covers the KEY bitmap up to ~KEY code 0x2FF
    let mut buf = [0u8; MAXB];
    let n = core::cmp::min(size, MAXB);
    match ev {
        0 => { // supported event types
            if n >= 1 {
                buf[0] = if dev_id == DEV_TABLET { 0x0B } else { 0x03 };
            }
        }
        1 => { // EV_KEY
            if dev_id == DEV_TABLET {
                // BTN_LEFT/RIGHT/MIDDLE = 0x110/0x111/0x112 → byte 34, bits 0..2.
                let byte = 0x110 >> 3;
                if byte < n { buf[byte] = 0x07; }
            } else {
                // keyboard advertises the full key range (as before).
                for b in buf[..n].iter_mut() { *b = 0xFF; }
            }
        }
        3 => { // EV_ABS
            if dev_id == DEV_TABLET && n >= 1 {
                buf[0] = 0x03; // ABS_X | ABS_Y
            }
        }
        _ => {} // EV_REL etc → none
    }
    copy_out(pid, arg_ptr, &buf[..n])
}

/// EVIOCGABS(abs): report input_absinfo for an absolute axis (tablet only).
fn eviocgabs(dev_id: usize, abs: usize, arg_ptr: usize, pid: u32) -> Message {
    // input_absinfo{ value, min, max, fuzz, flat, resolution } = 6×i32 = 24B.
    // Both axes: 0..32767, resolution 0 (libinput rejects X-xor-Y and mismatched
    // resolution).
    if dev_id == DEV_TABLET && (abs == ABS_X || abs == ABS_Y) {
        let info: [i32; 6] = [0, 0, 32767, 0, 0, 0];
        copy_out(pid, arg_ptr, unsafe {
            core::slice::from_raw_parts(info.as_ptr() as *const u8, 24)
        })
    } else {
        zero_out(pid, arg_ptr, 24)
    }
}

// ── Message Dispatch ──────────────────────────────────────────────────────────

pub fn handle(msg: &Message, _caller_pid: u32, _target_port: u32) -> Message {
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
            let pid = arg(msg, 3) as u32;
            let arg_ptr = arg(msg, 2) as usize;

            // ioctl request encoding: dir(2) size(14) type(8) nr(8).
            let nr   = cmd & 0xFF;
            let typ  = (cmd >> 8) & 0xFF;
            let size = (cmd >> 16) & 0x3FFF;

            if cmd == 0x541B { // FIONREAD (type 'T', not 'E')
                let count = (DEVICES.lock()[dev_id].count * core::mem::size_of::<input_event>()) as i32;
                return copy_out(pid, arg_ptr, &count.to_ne_bytes());
            }
            if typ != 0x45 { return err_reply(-25); } // not an 'E' ioctl → ENOTTY

            match nr {
                0x01 => val_reply(0x00010001), // EVIOCGVERSION
                0x02 => { // EVIOCGID → input_id{bustype,vendor,product,version} (8B)
                    let (vendor, product) = if dev_id == DEV_TABLET { (0x0627u16, 0x0001u16) }
                                            else { (0x0627u16, 0x0002u16) };
                    let ids: [u16; 4] = [BUS_VIRTUAL, vendor, product, 0x0001];
                    copy_out(pid, arg_ptr, unsafe {
                        core::slice::from_raw_parts(ids.as_ptr() as *const u8, 8)
                    })
                }
                0x06 => { // EVIOCGNAME(len)
                    let name: &[u8] = if dev_id == DEV_TABLET { b"QEMU Virtio Tablet\0" }
                                      else { b"QEMU Virtio Keyboard\0" };
                    let n = core::cmp::min(size, name.len());
                    copy_out(pid, arg_ptr, &name[..n])
                }
                0x07 | 0x08 => err_reply(-2), // EVIOCGPHYS/UNIQ → ENOENT (empty)
                0x09 => { // EVIOCGPROP → INPUT_PROP_POINTER for the tablet so libinput
                          // classifies it as an absolute POINTER (not a touchscreen)
                          // and delivers BTN_LEFT as a pointer button — required for
                          // click-to-focus (a compositor sets keyboard focus on a
                          // pointer button press). Keyboard advertises no props.
                    if dev_id == DEV_TABLET {
                        let mut buf = [0u8; 8];
                        buf[0] = 1 << 0; // bit 0 = INPUT_PROP_POINTER
                        let n = core::cmp::min(size, buf.len());
                        copy_out(pid, arg_ptr, &buf[..n])
                    } else {
                        zero_out(pid, arg_ptr, size)
                    }
                }
                0x18 | 0x19 | 0x1b => zero_out(pid, arg_ptr, size), // EVIOCGKEY/LED/SW → zeroed
                0xa0 => { // EVIOCSCLOCKID(int) — store; we already stamp monotonic
                    let mut clk = [0u8; 4];
                    if copy_in(pid, arg_ptr, &mut clk).is_none() { return err_reply(-14); }
                    let f = unsafe { arch_interrupt_save() };
                    DEVICES.lock()[dev_id].clockid = u32::from_ne_bytes(clk);
                    unsafe { arch_interrupt_restore(f); }
                    val_reply(0)
                }
                0x90 | 0x91 => val_reply(0), // EVIOCGRAB/EVIOCREVOKE → accept
                _ if (0x20..0x40).contains(&nr) => // EVIOCGBIT(ev, len)
                    eviocgbit(dev_id, nr - 0x20, arg_ptr, size, pid),
                _ if (0x40..0x60).contains(&nr) => // EVIOCGABS(abs)
                    eviocgabs(dev_id, nr - 0x40, arg_ptr, pid),
                _ => err_reply(-25), // ENOTTY
            }
        }
        vfs_server::VFS_POLL => {
            // POLLIN when any event is queued for this device (raw evdev fds read
            // whole input_event records, so a pending SYN is readable too). seq
            // is the push counter for epoll edge emulation.
            let f = unsafe { arch_interrupt_save() };
            let (count, seq) = { let d = &DEVICES.lock()[dev_id]; (d.count, d.seq) };
            unsafe { arch_interrupt_restore(f); }
            let revents: u32 = if count > 0 { 0x1 } else { 0 };
            let mut m = Message::empty();
            m.data[0..8].copy_from_slice(&(revents as u64).to_le_bytes());
            m.data[8..16].copy_from_slice(&seq.to_le_bytes());
            m
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

/// True if the pending queue holds at least one real key-down/serial event
/// (`type_ == EV_KEY`, `value == 1` or `2`) rather than only SYN markers or
/// key-release events. The kernel's `read_input_byte` silently discards
/// those non-actionable entries by popping and skipping them, so a lone
/// leftover SYN — always pushed right after every key event, including one
/// whose matching key-down byte a single-byte `read()` already consumed —
/// leaves `has_events()` true with nothing left that `read_input_byte`
/// would ever actually return. Used for fd 0's poll/epoll readiness check,
/// which must agree with what a following `read()` can really produce.
pub fn has_key_event(dev_id: u32) -> bool {
    if dev_id as usize >= MAX_DEVICES { return false; }
    let f = unsafe { arch_interrupt_save() };
    let devs = DEVICES.lock();
    let dev = &devs[dev_id as usize];
    let mut found = false;
    for i in 0..dev.count {
        let idx = (dev.head + i) % MAX_EVENTS;
        let ev = &dev.events[idx];
        if ev.type_ == 1 && (ev.value == 1 || ev.value == 2) {
            found = true;
            break;
        }
    }
    drop(devs);
    unsafe { arch_interrupt_restore(f); }
    found
}

pub fn push_event(dev_id: u32, type_: u16, code: u16, value: i32) {
    if dev_id as usize >= MAX_DEVICES { return; }

    // Stamp from the same monotonic clock userspace reads, at its full
    // resolution. libinput asks for CLOCK_MONOTONIC and compares event times
    // against its own clock_gettime(); everything downstream of it measures the
    // interval between events. A whole-tick stamp gave every event drained in
    // one 10 ms tick an identical timeval and a tv_usec that was always a
    // multiple of 10 000. Read it before masking interrupts and before taking
    // the device lock: push_event runs in IRQ context and this is two atomic
    // loads and a counter read — no locks and no user memory — but there is
    // still no reason to hold anything across it.
    let now_us = unsafe { arch_monotonic_ns() } / 1_000;
    EVENTS_PUSHED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let ev = input_event {
        time: timeval {
            tv_sec: (now_us / 1_000_000) as i64,
            tv_usec: (now_us % 1_000_000) as i64,
        },
        type_,
        code,
        value,
    };
    let f = unsafe { arch_interrupt_save() };
    let mut devs = DEVICES.lock();
    devs[dev_id as usize].push(ev);
    drop(devs);
    // A key event is a POLLIN edge for a console reader / evdev poller parked on
    // the poll wait-channel. try_wake (non-blocking) honors IRQ context; the
    // 100 Hz console-read tick is the backstop if RUN_QUEUE is momentarily busy.
    sched::try_wake_poll();
    unsafe { arch_interrupt_restore(f); }
}

pub fn init(owner_pid: u32) -> Option<u32> {
    let port_id = port::create(owner_pid)?;
    {
        let mut devs = DEVICES.lock();
        devs[DEV_KEYBOARD].in_use = true; // event0 (keyboard)
        devs[DEV_TABLET].in_use = true;   // event1 (virtio-tablet absolute pointer)
    }
    vfs_server::register_device("/dev/input/event0", port_id, 0, 13, 64);
    vfs_server::register_device("/dev/input/event1", port_id, 1, 13, 65);
    port::register_handler(port_id, handle);
    Some(port_id)
}
