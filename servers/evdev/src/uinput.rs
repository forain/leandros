//! /dev/uinput: Linux-compatible user-space input device creation.
//!
//! A daemon (the Bluetooth HID host) opens /dev/uinput, describes a device with
//! UI_SET_*BIT / UI_ABS_SETUP / UI_DEV_SETUP, creates it with UI_DEV_CREATE and
//! then write(2)s `struct input_event` records, which are pushed to the new
//! hot-plugged /dev/input/eventN node. Closing the fd (or UI_DEV_DESTROY)
//! removes the node. Per open context, keyed by the VFS open cookie.

use super::*;

pub const UINPUT_DEV_ID: u32 = MAX_DEVICES as u32 + 1;
const MAX_CTX: usize = 8;
const ABS_CODES: usize = 64;

#[derive(Clone, Copy)]
pub struct Ctx {
    in_use: bool,
    created: bool,
    open_id: u32,
    dev: u32,
    evbits: u32,
    key: [u8; KEY_BITMAP_BYTES],
    rel: u16,
    abs: u64,
    absinfo: [[i32; 6]; ABS_CODES],
    bus: u16,
    vendor: u16,
    product: u16,
    name: [u8; NAME_MAX],
    name_len: usize,
}

const CTX0: Ctx = Ctx {
    in_use: false, created: false, open_id: 0, dev: 0, evbits: 0,
    key: [0; KEY_BITMAP_BYTES], rel: 0, abs: 0, absinfo: [[0; 6]; ABS_CODES],
    bus: 0, vendor: 0, product: 0, name: [0; NAME_MAX], name_len: 0,
};

static CTXS: Mutex<[Ctx; MAX_CTX]> = Mutex::new([CTX0; MAX_CTX]);

/// Is `kind` one of the uinput-backed device kinds?
pub fn is_kind(kind: u8) -> bool {
    kind == DEVKIND_UINPUT || kind == DEVKIND_UINPUT_PAD || kind == DEVKIND_UINPUT_MOUSE
}

fn with_ctxs<R>(f: impl FnOnce(&mut [Ctx; MAX_CTX]) -> R) -> R {
    let fl = unsafe { arch_interrupt_save() };
    let r = f(&mut CTXS.lock());
    unsafe { arch_interrupt_restore(fl); }
    r
}

/// Fill the EVIOCGBIT bitmap of event type `ev` for the uinput node `dev_id`.
pub fn fill_bits(dev_id: usize, ev: usize, buf: &mut [u8]) {
    with_ctxs(|cs| {
        let c = match cs.iter().find(|c| c.in_use && c.created && c.dev == dev_id as u32) {
            Some(c) => c, None => return,
        };
        match ev {
            0 => for (i, b) in buf.iter_mut().take(4).enumerate() { *b = (c.evbits >> (8 * i)) as u8; },
            1 => { let n = buf.len().min(c.key.len()); buf[..n].copy_from_slice(&c.key[..n]); }
            2 => for (i, b) in buf.iter_mut().take(2).enumerate() { *b = (c.rel >> (8 * i)) as u8; },
            3 => for (i, b) in buf.iter_mut().take(8).enumerate() { *b = (c.abs >> (8 * i)) as u8; },
            _ => {}
        }
    });
}

/// input_absinfo of `abs` on the uinput node `dev_id` (value = last pushed).
pub fn absinfo(dev_id: usize, abs: usize) -> Option<[i32; 6]> {
    if abs >= ABS_CODES { return None; }
    with_ctxs(|cs| cs.iter().find(|c| c.in_use && c.created && c.dev == dev_id as u32)
        .map(|c| c.absinfo[abs]))
}

fn destroy_at(open_id: u32, only_node: bool) -> bool {
    let dev = with_ctxs(|cs| {
        let c = cs.iter_mut().find(|c| c.in_use && c.open_id == open_id)?;
        let d = if c.created { Some(c.dev) } else { None };
        if only_node { *c = Ctx { in_use: true, open_id, ..CTX0 }; } else { *c = CTX0; }
        Some(d)
    });
    match dev {
        Some(Some(d)) => { unregister_input_device(d); true }
        Some(None) => true,
        None => false,
    }
}

fn dev_of(open_id: u32) -> Option<u32> {
    with_ctxs(|cs| cs.iter().find(|c| c.in_use && c.open_id == open_id && c.created).map(|c| c.dev))
}

fn create(open_id: u32) -> Message {
    let (kind, name, nlen, bus, vendor, product) = match with_ctxs(|cs| {
        let c = cs.iter().find(|c| c.in_use && c.open_id == open_id)?;
        if c.created { return Some(None); }
        let kind = if c.key[0x120 / 8] & (1 << (0x120 % 8)) != 0 || c.key[0x130 / 8] & (1 << (0x130 % 8)) != 0 {
            DEVKIND_UINPUT_PAD
        } else if c.rel & 1 != 0 { DEVKIND_UINPUT_MOUSE } else { DEVKIND_UINPUT };
        Some(Some((kind, c.name, c.name_len, c.bus, c.vendor, c.product)))
    }) { Some(Some(t)) => t, Some(None) => return err_reply(-22), None => return err_reply(-9) };
    let nm = core::str::from_utf8(&name[..nlen]).unwrap_or("uinput");
    let nm = if nm.is_empty() { "uinput" } else { nm };
    let dev = match register_input_device(kind, nm, bus, vendor, product) {
        Some(d) => d, None => return err_reply(-12),
    };
    let ok = with_ctxs(|cs| match cs.iter_mut().find(|c| c.in_use && c.open_id == open_id) {
        Some(c) => { c.created = true; c.dev = dev; true }
        None => false,
    });
    if !ok { unregister_input_device(dev); return err_reply(-9); }
    val_reply(0)
}

fn do_write(msg: &Message, open_id: u32, pid: u32) -> Message {
    let buf_ptr = arg(msg, 1) as usize;
    let count = arg(msg, 2) as usize;
    let dev = match dev_of(open_id) { Some(d) => d, None => return err_reply(-22) };
    const SZ: usize = 24;
    if count < SZ { return err_reply(-22); }
    let total = count / SZ * SZ;
    let mut done = 0usize;
    while done < total {
        let mut raw = [0u8; SZ * 16];
        let n = (total - done).min(raw.len());
        if copy_in(pid, buf_ptr + done, &mut raw[..n]).is_none() {
            return if done > 0 { val_reply(done as u64) } else { err_reply(-14) };
        }
        for r in raw[..n].chunks_exact(SZ) {
            let t = u16::from_le_bytes([r[16], r[17]]);
            let c = u16::from_le_bytes([r[18], r[19]]);
            let v = i32::from_le_bytes([r[20], r[21], r[22], r[23]]);
            if t == 3 && (c as usize) < ABS_CODES {
                with_ctxs(|cs| if let Some(x) = cs.iter_mut().find(|x| x.in_use && x.open_id == open_id) {
                    x.absinfo[c as usize][0] = v;
                });
            }
            push_event(dev, t, c, v);
        }
        done += n;
    }
    val_reply(done as u64)
}

fn do_ioctl(msg: &Message, open_id: u32) -> Message {
    let cmd = arg(msg, 1) as usize;
    let arg_ptr = arg(msg, 2) as usize;
    let pid = arg(msg, 3) as u32;
    if (cmd >> 8) & 0xFF != 0x55 { return err_reply(-25); }
    let nr = cmd & 0xFF;
    let set_bit = |f: &mut dyn FnMut(&mut Ctx) -> bool| -> Message {
        if with_ctxs(|cs| cs.iter_mut().find(|c| c.in_use && c.open_id == open_id).map_or(false, |c| f(c))) {
            val_reply(0)
        } else { err_reply(-22) }
    };
    let v = arg_ptr;
    match nr {
        1 => create(open_id),
        2 => { if destroy_at(open_id, true) { val_reply(0) } else { err_reply(-9) } }
        3 => { // UI_DEV_SETUP
            let mut b = [0u8; 92];
            if copy_in(pid, arg_ptr, &mut b).is_none() { return err_reply(-14); }
            let (bus, vendor, product) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]), u16::from_le_bytes([b[4], b[5]]));
            let nm = &b[8..88];
            let l = nm.iter().position(|&x| x == 0).unwrap_or(80).min(NAME_MAX - 1);
            set_bit(&mut |c| {
                if c.created { return false; }
                c.bus = bus; c.vendor = vendor; c.product = product;
                c.name = [0; NAME_MAX]; c.name[..l].copy_from_slice(&nm[..l]); c.name_len = l;
                true
            })
        }
        4 => { // UI_ABS_SETUP
            let mut b = [0u8; 28];
            if copy_in(pid, arg_ptr, &mut b).is_none() { return err_reply(-14); }
            let code = u16::from_le_bytes([b[0], b[1]]) as usize;
            if code >= ABS_CODES { return err_reply(-22); }
            let mut info = [0i32; 6];
            for (i, x) in info.iter_mut().enumerate() {
                *x = i32::from_le_bytes([b[4 + 4 * i], b[5 + 4 * i], b[6 + 4 * i], b[7 + 4 * i]]);
            }
            set_bit(&mut |c| { if c.created { return false; } c.absinfo[code] = info; true })
        }
        44 => { // UI_GET_SYSNAME(len)
            let size = (cmd >> 16) & 0x3FFF;
            let d = match dev_of(open_id) { Some(d) => d, None => return err_reply(-22) };
            let mut s = [0u8; 16];
            s[..5].copy_from_slice(b"event");
            let mut n = 5;
            if d >= 10 { s[n] = b'0' + (d / 10) as u8; n += 1; }
            s[n] = b'0' + (d % 10) as u8; n += 1;
            let n = (n + 1).min(size);
            copy_out(pid, arg_ptr, &s[..n])
        }
        45 => copy_out(pid, arg_ptr, &5u32.to_ne_bytes()), // UI_GET_VERSION
        100 => { if v >= 32 { return err_reply(-22); } set_bit(&mut |c| { if c.created { return false; } c.evbits |= 1 << v; true }) }
        101 => { if v >= KEY_BITMAP_BYTES * 8 { return err_reply(-22); } set_bit(&mut |c| { if c.created { return false; } c.key[v / 8] |= 1 << (v % 8); true }) }
        102 => { if v >= 16 { return err_reply(-22); } set_bit(&mut |c| { if c.created { return false; } c.rel |= 1 << v; true }) }
        103 => { if v >= ABS_CODES { return err_reply(-22); } set_bit(&mut |c| { if c.created { return false; } c.abs |= 1 << v; true }) }
        _ => err_reply(-25),
    }
}

pub fn handle(msg: &Message) -> Message {
    let (open_id, pid) = client_key(msg);
    match msg.tag {
        vfs_server::VFS_OPEN => {
            let ok = with_ctxs(|cs| match cs.iter_mut().find(|c| !c.in_use) {
                Some(c) => { *c = CTX0; c.in_use = true; c.open_id = open_id; true }
                None => false,
            });
            if ok { val_reply(0) } else { err_reply(-24) }
        }
        vfs_server::VFS_WRITE => do_write(msg, open_id, pid),
        vfs_server::VFS_IOCTL => do_ioctl(msg, arg(msg, 4) as u32),
        vfs_server::VFS_READ => err_reply(-22),
        vfs_server::VFS_POLL => {
            let mut m = Message::empty();
            m.data[0..8].copy_from_slice(&4u64.to_le_bytes()); // POLLOUT
            m
        }
        vfs_server::VFS_CLOSE => {
            let oid = arg(msg, 1) as u32;
            if oid != 0 { destroy_at(oid, false); }
            val_reply(0)
        }
        _ => err_reply(-38),
    }
}
