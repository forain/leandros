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
/// Force-feedback effect slots per device (Linux caps ff_effects_max at
/// FF_MAX_EFFECTS = 96; the table here holds 32, so larger asks are clamped).
const FF_SLOTS: usize = 32;
const FF_EFFECT_SZ: usize = 48; // sizeof(struct ff_effect), LP64 (both arches)
const EV_SZ: usize = 24;        // sizeof(struct input_event), LP64
const RING: usize = 32;
const EV_UINPUT: u16 = 0x0101;
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;
/// Linux UINPUT_TIMEOUT: how long an EVIOCSFF/EVIOCRMFF waits for the owner.
const FF_TIMEOUT_NS: u64 = 30_000_000_000;
static NEXT_REQ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

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
    prop: u32,
    absinfo: [[i32; 6]; ABS_CODES],
    bus: u16,
    vendor: u16,
    product: u16,
    name: [u8; NAME_MAX],
    name_len: usize,
    ff_max: u32,
    ff_bits: [u8; 16],
    eff_used: u32,
    eff_owner: [u32; FF_SLOTS],
    eff: [[u8; FF_EFFECT_SZ]; FF_SLOTS],
    /// Single outstanding upload/erase request: 0 idle, 1 queued for the
    /// owner, 2 taken (UI_BEGIN_*), 3 completed (UI_END_*).
    req_state: u8,
    req_kind: u16,
    req_id: u32,
    req_eid: i32,
    req_new: [u8; FF_EFFECT_SZ],
    req_old: [u8; FF_EFFECT_SZ],
    req_ret: i32,
    /// Events for the owner to read(2): requests and EV_FF play/gain.
    ring: [[u8; EV_SZ]; RING],
    rhead: usize,
    rcount: usize,
    seq: u64,
}

const CTX0: Ctx = Ctx {
    in_use: false, created: false, open_id: 0, dev: 0, evbits: 0,
    key: [0; KEY_BITMAP_BYTES], rel: 0, abs: 0, prop: 0, absinfo: [[0; 6]; ABS_CODES],
    bus: 0, vendor: 0, product: 0, name: [0; NAME_MAX], name_len: 0,
    ff_max: 0, ff_bits: [0; 16], eff_used: 0, eff_owner: [0; FF_SLOTS],
    eff: [[0; FF_EFFECT_SZ]; FF_SLOTS],
    req_state: 0, req_kind: 0, req_id: 0, req_eid: 0,
    req_new: [0; FF_EFFECT_SZ], req_old: [0; FF_EFFECT_SZ], req_ret: 0,
    ring: [[0; EV_SZ]; RING], rhead: 0, rcount: 0, seq: 0,
};

static CTXS: Mutex<[Ctx; MAX_CTX]> = Mutex::new([CTX0; MAX_CTX]);

/// Is `kind` one of the uinput-backed device kinds?
pub fn is_kind(kind: u8) -> bool {
    kind == DEVKIND_UINPUT || kind == DEVKIND_UINPUT_PAD || kind == DEVKIND_UINPUT_MOUSE
        || kind == DEVKIND_UINPUT_TOUCHPAD
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
            0x15 => { let n = buf.len().min(16); buf[..n].copy_from_slice(&c.ff_bits[..n]); }
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

/// INPUT_PROP_* bitmask (first 32 props) of the uinput node `dev_id`.
pub fn props(dev_id: usize) -> u32 {
    with_ctxs(|cs| cs.iter().find(|c| c.in_use && c.created && c.dev == dev_id as u32).map_or(0, |c| c.prop))
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
        let mt = c.abs & (1u64 << 0x2f) != 0 && c.abs & (1u64 << 0x35) != 0;
        let kind = if mt { DEVKIND_UINPUT_TOUCHPAD } else if c.key[0x120 / 8] & (1 << (0x120 % 8)) != 0 || c.key[0x130 / 8] & (1 << (0x130 % 8)) != 0 {
            DEVKIND_UINPUT_PAD
        } else if c.rel & 1 != 0 { DEVKIND_UINPUT_MOUSE } else { DEVKIND_UINPUT };
        // Linux uinput_create_device: EV_FF without ff_effects_max is invalid.
        if c.evbits & (1 << 0x15) != 0 && c.ff_max == 0 { return Some(None); }
        Some(Some((kind, c.name, c.name_len, c.bus, c.vendor, c.product)))
    }) { Some(Some(t)) => t, Some(None) => return err_reply(-22), None => return err_reply(-9) };
    let nm = core::str::from_utf8(&name[..nlen]).unwrap_or("uinput");
    let nm = if nm.is_empty() { "uinput" } else { nm };
    let dev = match register_input_device(kind, nm, bus, vendor, product) {
        Some(d) => d, None => return err_reply(-12),
    };
    if kind == DEVKIND_UINPUT_TOUCHPAD {
        let n = with_ctxs(|cs| cs.iter().find(|c| c.in_use && c.open_id == open_id)
            .map_or(0, |c| c.absinfo[0x2f][2] + 1));
        mt_init(dev, n.max(1) as usize);
    }
    let ok = with_ctxs(|cs| match cs.iter_mut().find(|c| c.in_use && c.open_id == open_id) {
        Some(c) => { c.created = true; c.dev = dev; true }
        None => false,
    });
    if !ok { unregister_input_device(dev); return err_reply(-9); }
    val_reply(0)
}

fn q_push(c: &mut Ctx, t: u16, code: u16, v: i32) {
    if c.rcount >= RING { return; }
    let ns = unsafe { arch_monotonic_ns() };
    let mut r = [0u8; EV_SZ];
    r[0..8].copy_from_slice(&((ns / 1_000_000_000) as i64).to_le_bytes());
    r[8..16].copy_from_slice(&(((ns / 1_000) % 1_000_000) as i64).to_le_bytes());
    r[16..18].copy_from_slice(&t.to_le_bytes());
    r[18..20].copy_from_slice(&code.to_le_bytes());
    r[20..24].copy_from_slice(&v.to_le_bytes());
    let i = (c.rhead + c.rcount) % RING;
    c.ring[i] = r;
    c.rcount += 1;
    c.seq += 1;
}

fn wake_owner() {
    for _ in 0..4 {
        if sched::try_wake_poll_key(evdev_poll_key(UINPUT_DEV_ID)) { break; }
    }
}

/// Legacy `struct uinput_user_dev` (1116 bytes) written before UI_DEV_CREATE:
/// name[80], input_id, ff_effects_max, absmax/absmin/absfuzz/absflat[64].
fn legacy_setup(open_id: u32, pid: u32, buf_ptr: usize, count: usize) -> Message {
    const LSZ: usize = 1116;
    if count < LSZ { return err_reply(-22); }
    let mut b = [0u8; LSZ];
    if copy_in(pid, buf_ptr, &mut b).is_none() { return err_reply(-14); }
    let nm = &b[0..80];
    let l = nm.iter().position(|&x| x == 0).unwrap_or(80).min(NAME_MAX - 1);
    let rd = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let ffm = rd(92);
    if ffm < 0 || ffm > 96 { return err_reply(-22); }
    let ok = with_ctxs(|cs| match cs.iter_mut().find(|c| c.in_use && c.open_id == open_id) {
        Some(c) if !c.created => {
            c.bus = u16::from_le_bytes([b[80], b[81]]);
            c.vendor = u16::from_le_bytes([b[82], b[83]]);
            c.product = u16::from_le_bytes([b[84], b[85]]);
            c.name = [0; NAME_MAX]; c.name[..l].copy_from_slice(&nm[..l]); c.name_len = l;
            c.ff_max = (ffm as u32).min(FF_SLOTS as u32);
            for a in 0..ABS_CODES {
                if c.abs & (1u64 << a) == 0 { continue; }
                // absinfo = [value, min, max, fuzz, flat, resolution]
                c.absinfo[a] = [0, rd(96 + 256 + a * 4), rd(96 + a * 4), rd(96 + 512 + a * 4), rd(96 + 768 + a * 4), 0];
            }
            true
        }
        _ => false,
    });
    if ok { val_reply(count as u64) } else { err_reply(-22) }
}

fn do_write(msg: &Message, open_id: u32, pid: u32) -> Message {
    let buf_ptr = arg(msg, 1) as usize;
    let count = arg(msg, 2) as usize;
    let dev = match dev_of(open_id) {
        Some(d) => d,
        None => return legacy_setup(open_id, pid, buf_ptr, count),
    };
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

/// read(2) on the uinput fd: queued input_event records (UI_FF_UPLOAD /
/// UI_FF_ERASE requests as EV_UINPUT, EV_FF play and gain). Empty -> EAGAIN.
fn do_read(msg: &Message, open_id: u32, pid: u32) -> Message {
    let buf_ptr = arg(msg, 1) as usize;
    let count = arg(msg, 2) as usize;
    if count < EV_SZ { return err_reply(-22); }
    let want = (count / EV_SZ).min(RING);
    let mut out = [0u8; EV_SZ * RING];
    let r = with_ctxs(|cs| {
        let c = cs.iter_mut().find(|c| c.in_use && c.open_id == open_id)?;
        if !c.created { return Some(Err(-19)); }
        let n = want.min(c.rcount);
        for i in 0..n { out[i * EV_SZ..(i + 1) * EV_SZ].copy_from_slice(&c.ring[(c.rhead + i) % RING]); }
        c.rhead = (c.rhead + n) % RING;
        c.rcount -= n;
        Some(Ok(n))
    });
    match r {
        None => err_reply(-9),
        Some(Err(e)) => err_reply(e),
        Some(Ok(0)) => err_reply(-11),
        Some(Ok(n)) => {
            if sched::copy_to_task(pid, buf_ptr, &out[..n * EV_SZ]) { val_reply((n * EV_SZ) as u64) } else { err_reply(-14) }
        }
    }
}

fn find_dev<'a>(cs: &'a mut [Ctx; MAX_CTX], dev: usize) -> Option<&'a mut Ctx> {
    cs.iter_mut().find(|c| c.in_use && c.created && c.dev == dev as u32)
}

/// Queue an upload/erase request for the uinput owner and wait for its
/// UI_END_FF_*; Linux `uinput_request_submit` (30 s timeout, ENODEV if the
/// device goes away). Runs in the requesting task's context, no lock held
/// while waiting.
fn submit(dev: usize, kind: u16, new: &[u8; FF_EFFECT_SZ], old: &[u8; FF_EFFECT_SZ], eid: i32) -> i32 {
    let start = unsafe { arch_monotonic_ns() };
    let rid = NEXT_REQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    loop {
        let r = with_ctxs(|cs| {
            let c = match find_dev(cs, dev) { Some(c) => c, None => return Err(-19) };
            if c.req_state != 0 { return Ok(false); }
            c.req_state = 1; c.req_kind = kind; c.req_id = rid; c.req_eid = eid;
            c.req_new = *new; c.req_old = *old; c.req_ret = 0;
            q_push(c, EV_UINPUT, kind, rid as i32);
            Ok(true)
        });
        match r {
            Err(e) => return e,
            Ok(true) => break,
            Ok(false) => {
                if unsafe { arch_monotonic_ns() } - start > FF_TIMEOUT_NS { return -110; }
                sched::yield_now("uinput-ff");
            }
        }
    }
    wake_owner();
    loop {
        let r = with_ctxs(|cs| {
            let c = match find_dev(cs, dev) { Some(c) => c, None => return Err(-19) };
            if c.req_id != rid || c.req_state == 0 { return Err(-19); }
            if c.req_state == 3 { let v = c.req_ret; c.req_state = 0; return Ok(Some(v)); }
            Ok(None)
        });
        match r {
            Err(e) => return e,
            Ok(Some(v)) => return v,
            Ok(None) => {
                if unsafe { arch_monotonic_ns() } - start > FF_TIMEOUT_NS {
                    with_ctxs(|cs| if let Some(c) = find_dev(cs, dev) { if c.req_id == rid { c.req_state = 0; } });
                    return -110;
                }
                sched::yield_now("uinput-ff");
            }
        }
    }
}

/// Linux `check_effect_access`.
fn eff_access(c: &Ctx, id: i32, open_id: u32) -> i32 {
    if id < 0 || id as u32 >= c.ff_max || c.eff_used & (1 << id) == 0 { return -22; }
    if c.eff_owner[id as usize] != open_id { return -13; }
    0
}

/// Event-node ioctls EVIOCSFF (0x80), EVIOCRMFF (0x81), EVIOCGEFFECTS (0x84)
/// on a uinput device; Linux `input_ff_upload`/`input_ff_erase` semantics.
pub fn eviocff(dev: usize, nr: usize, open_id: u32, pid: u32, arg_ptr: usize) -> Message {
    let (ffm, ffbits) = match with_ctxs(|cs| find_dev(cs, dev).map(|c| (c.ff_max, c.ff_bits))) {
        Some(t) => t, None => return err_reply(-19),
    };
    let done = |r: i32| if r == 0 { val_reply(0) } else { err_reply(r) };
    match nr {
        0x84 => { let w = copy_out(pid, arg_ptr, &(ffm as i32).to_ne_bytes()); if w.data[0..8] == val_reply(4).data[0..8] { val_reply(0) } else { w } }
        _ if ffm == 0 => err_reply(-38), // no EV_FF: ENOSYS
        0x80 => {
            let mut e = [0u8; FF_EFFECT_SZ];
            if copy_in(pid, arg_ptr, &mut e).is_none() { return err_reply(-14); }
            let ty = u16::from_le_bytes([e[0], e[1]]) as usize;
            let mut id = i16::from_le_bytes([e[2], e[3]]) as i32;
            let bit = |b: usize| b < 128 && ffbits[b / 8] & (1 << (b % 8)) != 0;
            if !(0x50..=0x57).contains(&ty) || !bit(ty) { return err_reply(-22); }
            if ty == 0x51 {
                let wf = u16::from_le_bytes([e[16], e[17]]) as usize;
                if !(0x58..=0x5d).contains(&wf) || !bit(wf) { return err_reply(-22); }
            }
            if id < -1 || id >= ffm as i32 { return err_reply(-22); }
            let mut old = [0u8; FF_EFFECT_SZ];
            let is_new = id < 0;
            // Reserve a slot (new) or check ownership (replace) under one lock.
            let rc = with_ctxs(|cs| {
                let c = match find_dev(cs, dev) { Some(c) => c, None => return -19 };
                if is_new {
                    match (0..ffm as usize).find(|&i| c.eff_used & (1 << i) == 0) {
                        Some(i) => { c.eff_used |= 1 << i; c.eff_owner[i] = open_id; id = i as i32; 0 }
                        None => -28, // ENOSPC
                    }
                } else {
                    let a = eff_access(c, id, open_id);
                    if a == 0 { old = c.eff[id as usize]; }
                    a
                }
            });
            if rc != 0 { return err_reply(rc); }
            e[2..4].copy_from_slice(&(id as i16).to_le_bytes());
            let r = submit(dev, UI_FF_UPLOAD, &e, &old, 0);
            with_ctxs(|cs| if let Some(c) = find_dev(cs, dev) {
                if r == 0 { c.eff[id as usize] = e; }
                else if is_new { c.eff_used &= !(1 << id); }
            });
            if r != 0 { return err_reply(r); }
            // Report the assigned id back, as evdev's EVIOCSFF does.
            let w = copy_out(pid, arg_ptr + 2, &(id as i16).to_le_bytes());
            if w.data[0..8] != val_reply(2).data[0..8] { return w; }
            val_reply(0)
        }
        0x81 => {
            let id = arg_ptr as u32 as i32;
            let rc = with_ctxs(|cs| match find_dev(cs, dev) { Some(c) => eff_access(c, id, open_id), None => -19 });
            if rc != 0 { return err_reply(rc); }
            let z = [0u8; FF_EFFECT_SZ];
            let r = submit(dev, UI_FF_ERASE, &z, &z, id);
            if r == 0 { with_ctxs(|cs| if let Some(c) = find_dev(cs, dev) { c.eff_used &= !(1 << id); }); }
            done(r)
        }
        _ => err_reply(-25),
    }
}

/// An event-node open went away: drop the effects it uploaded (Linux also
/// asks the owner to erase them; here the owner is not consulted).
pub fn ff_flush(open_id: u32) {
    with_ctxs(|cs| for c in cs.iter_mut() {
        for i in 0..FF_SLOTS {
            if c.eff_used & (1 << i) != 0 && c.eff_owner[i] == open_id { c.eff_used &= !(1 << i); }
        }
    });
}

/// write(2) on a uinput event node: EV_FF records (play/stop, FF_GAIN,
/// FF_AUTOCENTER) are delivered to the owner's fd, as `uinput_dev_event` does.
pub fn ff_write(dev: usize, pid: u32, buf_ptr: usize, count: usize) {
    let total = (count / EV_SZ * EV_SZ).min(EV_SZ * 64);
    let mut done = 0;
    let mut woke = false;
    while done < total {
        let mut raw = [0u8; EV_SZ * 16];
        let n = (total - done).min(raw.len());
        if copy_in(pid, buf_ptr + done, &mut raw[..n]).is_none() { break; }
        for r in raw[..n].chunks_exact(EV_SZ) {
            let t = u16::from_le_bytes([r[16], r[17]]);
            let code = u16::from_le_bytes([r[18], r[19]]);
            let v = i32::from_le_bytes([r[20], r[21], r[22], r[23]]);
            if t != 0x15 || v < 0 { continue; }
            with_ctxs(|cs| if let Some(c) = find_dev(cs, dev) {
                if c.ff_max == 0 { return; }
                if code < 0x60 && (code as u32 >= c.ff_max || c.eff_used & (1 << code) == 0) { return; }
                if code > 0x61 && code >= 0x60 { return; }
                q_push(c, 0x15, code, v);
                woke = true;
            });
        }
        done += n;
    }
    if woke { wake_owner(); }
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
            let ffm = u32::from_le_bytes([b[88], b[89], b[90], b[91]]);
            if ffm > 96 { return err_reply(-22); }
            set_bit(&mut |c| {
                if c.created { return false; }
                c.ff_max = ffm.min(FF_SLOTS as u32);
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
        110 => { if v >= 32 { return err_reply(-22); } set_bit(&mut |c| { if c.created { return false; } c.prop |= 1 << v; true }) }
        107 => { // UI_SET_FFBIT
            if v >= 128 { return err_reply(-22); }
            set_bit(&mut |c| { if c.created { return false; } c.ff_bits[v / 8] |= 1 << (v % 8); true })
        }
        200 | 202 => { // UI_BEGIN_FF_UPLOAD / UI_BEGIN_FF_ERASE
            let upload = nr == 200;
            let mut idb = [0u8; 4];
            if copy_in(pid, arg_ptr, &mut idb).is_none() { return err_reply(-14); }
            let id = u32::from_le_bytes(idb);
            let out = with_ctxs(|cs| {
                let c = cs.iter_mut().find(|c| c.in_use && c.open_id == open_id && c.created)?;
                let want = if upload { UI_FF_UPLOAD } else { UI_FF_ERASE };
                if (c.req_state != 1 && c.req_state != 2) || c.req_id != id || c.req_kind != want { return None; }
                c.req_state = 2;
                let mut o = [0u8; 2 * FF_EFFECT_SZ + 8];
                o[0..4].copy_from_slice(&id.to_le_bytes());
                if upload {
                    o[8..8 + FF_EFFECT_SZ].copy_from_slice(&c.req_new);
                    o[8 + FF_EFFECT_SZ..].copy_from_slice(&c.req_old);
                    Some((o, 8 + 2 * FF_EFFECT_SZ))
                } else {
                    o[8..12].copy_from_slice(&c.req_eid.to_le_bytes());
                    Some((o, 12))
                }
            });
            match out {
                Some((o, n)) => { let r = copy_out(pid, arg_ptr, &o[..n]); if r.data[0..8] == val_reply(n as u64).data[0..8] { val_reply(0) } else { r } }
                None => err_reply(-22),
            }
        }
        201 | 203 => { // UI_END_FF_UPLOAD / UI_END_FF_ERASE
            let want = if nr == 201 { UI_FF_UPLOAD } else { UI_FF_ERASE };
            let mut hb = [0u8; 8];
            if copy_in(pid, arg_ptr, &mut hb).is_none() { return err_reply(-14); }
            let id = u32::from_le_bytes([hb[0], hb[1], hb[2], hb[3]]);
            let ret = i32::from_le_bytes([hb[4], hb[5], hb[6], hb[7]]);
            let ok = with_ctxs(|cs| match cs.iter_mut().find(|c| c.in_use && c.open_id == open_id && c.created) {
                Some(c) if (c.req_state == 1 || c.req_state == 2) && c.req_id == id && c.req_kind == want => {
                    c.req_ret = ret; c.req_state = 3; true
                }
                _ => false,
            });
            if ok { val_reply(0) } else { err_reply(-22) }
        }
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
        vfs_server::VFS_READ => do_read(msg, open_id, pid),
        vfs_server::VFS_POLL => {
            let (n, seq, created) = with_ctxs(|cs| cs.iter().find(|c| c.in_use && c.open_id == open_id)
                .map_or((0, 0, false), |c| (c.rcount, c.seq, c.created)));
            let mut m = Message::empty();
            let rev: u64 = 4 | if created && n > 0 { 1 } else { 0 }; // POLLOUT | POLLIN
            m.data[0..8].copy_from_slice(&rev.to_le_bytes());
            m.data[8..16].copy_from_slice(&seq.to_le_bytes());
            let key = evdev_poll_key(UINPUT_DEV_ID);
            m.data[24..32].copy_from_slice(&sched::poll_key_bit(key).to_le_bytes());
            m.data[32] = 1;
            sched::poll_reply_put_keys(&mut m.data, &[key]);
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
