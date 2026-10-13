//! PS/2 mouse / touchpad on the i8042 AUX port: probe and setup (task
//! context, polled). Packet decoding is `drivers::psmouse`.
//!
//! Sequence (logged line by line with the `[PSMOUSE]` prefix, so one boot of
//! an unknown laptop tells what its pointing devices are):
//!
//! 1. Enable the AUX clock (0xA8) and run the AUX interface test (0xA9).
//! 2. Reset the device (0xFF): ACK, self-test 0xAA, device ID.
//! 3. Synaptics identify (E6, 4×E8 "sliced" 0x00, E9): middle byte 0x47.
//!    If so: model, capabilities, modes, extended capability queries,
//!    coordinate ranges, and pass-through capability are logged, and the
//!    pad is put in absolute W mode (0xC5) with the pass-through device
//!    enabled. Any failure falls back to relative mode.
//! 4. Otherwise (or forced): IntelliMouse Explorer knock (200,200,80 → ID 4),
//!    then ImPS/2 (200,100,80 → ID 3), else bare PS/2.
//! 5. Rate 100 (relative), resolution 8/mm, scaling 1:1, enable (0xF4); AUX
//!    IRQ on in the command byte and IRQ 12 routed to vector 33, which the
//!    keyboard IRQ already uses: one handler drains the controller and splits
//!    bytes by the status AUX bit (as Linux's single i8042 lock does).
//!
//! Active multiplexing (Linux `i8042_check_mux`): before step 2 the AUX
//! loopback (0xD3) is fed F0, 56, A4; a controller that answers the last
//! byte with something else is a MUX controller and that byte is its
//! version. Each of AUX0..AUX3 is then enabled (0x90+n, 0xA8), addressed
//! with 0x90+n instead of 0xD4, and probed and decoded on its own (steps
//! 2-5 per port): e.g. HP EliteBooks put the stick on AUX1 and the
//! Synaptics pad on AUX2. A relative device next to a touchpad is driven as
//! a pointing stick (middle-button scrolling). `i8042.nomux=1` keeps the
//! legacy single AUX port.
//!
//! `psmouse.proto=imps|exps|bare|synaptics|off` forces a protocol (`off`
//! leaves AUX untouched); `psmouse.debug=1` dumps raw packets.

use core::sync::atomic::Ordering;
use drivers::psmouse::{self as pm, SynInfo};
use super::keyboard::{self as kb, inb, outb, DATA, STATUS, ST_AUX, ST_MUXERR, ST_OBF, ST_PARITY, ST_TIMEOUT, WAIT_POLLS};
use core::sync::atomic::AtomicU8;

/// Port the polled probe talks to: 0..=3 mux port, `pm::LEGACY` otherwise.
static PORT: AtomicU8 = AtomicU8::new(pm::LEGACY as u8);

fn port() -> u8 { PORT.load(Ordering::Relaxed) }
fn muxed() -> bool { (port() as usize) < pm::LEGACY }

extern "C" {
    fn serial_print(s: *const u8, len: usize);
}

fn log(s: &str) { unsafe { serial_print(s.as_ptr(), s.len()) }; }

fn hex(v: u32, digits: u32) {
    let mut b = [0u8; 8];
    for i in 0..digits as usize {
        let d = ((v >> (4 * (digits as usize - 1 - i))) & 15) as u8;
        b[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
    }
    log(core::str::from_utf8(&b[..digits as usize]).unwrap_or("?"));
}

fn dec(v: u32) {
    let mut b = [0u8; 10];
    let mut i = b.len();
    let mut v = v;
    loop { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    log(core::str::from_utf8(&b[i..]).unwrap_or("?"));
}

/// Read one AUX byte; keyboard bytes arriving meanwhile are dropped.
/// `tries` × ~100 ms on hardware.
unsafe fn aux_read(tries: u32) -> Option<u8> {
    for _ in 0..tries {
        for _ in 0..WAIT_POLLS {
            let st = inb(STATUS);
            if st == 0xFF { return None; }
            if st & ST_OBF != 0 {
                let b = inb(DATA);
                if st & ST_AUX == 0 { continue; }
                if muxed() {
                    // MUX error: 0xFC-0xFF = timeout (nothing on the port);
                    // anything else is our data with a confused port field.
                    if st & ST_MUXERR != 0 { if b >= 0xFC { return None; } return Some(b); }
                    if (st >> 6) & 3 == port() { return Some(b); }
                    continue; // another (already enabled) port
                }
                if st & (ST_TIMEOUT | ST_PARITY) == 0 { return Some(b); }
                return None;
            }
            core::hint::spin_loop();
        }
    }
    None
}

unsafe fn aux_write(b: u8) -> bool {
    let c = if muxed() { 0x90 + port() } else { 0xD4 };
    if !kb::command(c) || !kb::wait_write() { return false; }
    outb(DATA, b);
    true
}

/// Send one byte to the device and wait for its ACK (one resend on 0xFE).
unsafe fn aux_send(b: u8) -> Result<(), u8> {
    for _ in 0..2 {
        if !aux_write(b) { return Err(0); }
        match aux_read(3) {
            Some(0xFA) => return Ok(()),
            Some(0xFE) => continue,
            Some(x) => return Err(x),
            None => return Err(0),
        }
    }
    Err(0xFE)
}

/// Command with an optional parameter byte and `resp.len()` reply bytes.
unsafe fn aux_cmd(cmd: u8, param: Option<u8>, resp: &mut [u8]) -> bool {
    if aux_send(cmd).is_err() { return false; }
    if let Some(p) = param { if aux_send(p).is_err() { return false; } }
    for r in resp.iter_mut() {
        match aux_read(3) { Some(b) => *r = b, None => return false }
    }
    true
}

/// Synaptics "sliced" command: E6, then the byte two bits at a time in E8s.
unsafe fn sliced(c: u8) -> bool {
    if !aux_cmd(0xE6, None, &mut []) { return false; }
    for i in [6u8, 4, 2, 0] {
        if !aux_cmd(0xE8, Some((c >> i) & 3), &mut []) { return false; }
    }
    true
}

/// Synaptics information query: sliced `q`, then E9 → 3 bytes.
unsafe fn syn_query(q: u8) -> Option<[u8; 3]> {
    let mut r = [0u8; 3];
    if sliced(q) && aux_cmd(0xE9, None, &mut r) { Some(r) } else { None }
}

fn log3(r: &[u8; 3]) { hex(r[0] as u32, 2); log(" "); hex(r[1] as u32, 2); log(" "); hex(r[2] as u32, 2); }

unsafe fn reset() -> Option<u8> {
    if aux_send(0xFF).is_err() { return None; }
    // Self-test takes up to ~1 s on touchpads.
    let bat = aux_read(20)?;
    let id = aux_read(5).unwrap_or(0xFF);
    log("[PSMOUSE] reset: BAT 0x"); hex(bat as u32, 2); log(" id 0x"); hex(id as u32, 2); log("\n");
    if bat != 0xAA { return None; }
    Some(id)
}

unsafe fn get_id() -> Option<u8> {
    let mut r = [0u8; 1];
    if aux_cmd(0xF2, None, &mut r) { Some(r[0]) } else { None }
}

unsafe fn knock(a: u8, b: u8, c: u8) -> Option<u8> {
    for r in [a, b, c] { if !aux_cmd(0xF3, Some(r), &mut []) { return None; } }
    get_id()
}

/// Probe and set up the AUX device. Task context, after `keyboard::recheck`
/// and after the kernel command line is available to `drivers`.
pub fn init() {
    pm::configure();
    if !kb::present() { log("[PSMOUSE] no i8042 controller: no PS/2 pointer\n"); return; }
    let forced = pm::forced_protocol();
    if forced == Some(pm::PROTO_OFF) { log("[PSMOUSE] psmouse.proto=off: AUX port left alone\n"); return; }
    kb::AUX_BUSY.store(true, Ordering::Release);
    let r = unsafe { setup(forced) };
    unsafe { let _ = kb::flush(); }
    kb::AUX_BUSY.store(false, Ordering::Release);
    match r {
        Ok(()) => {
            unsafe { super::ioapic::set_irq(12, super::cpus::bsp_apic_id().min(0xFF) as u8, 33) };
            log("[PSMOUSE] IRQ 12 -> vector 33\n");
        }
        Err(why) => {
            kb::MUX.store(false, Ordering::Release);
            for p in 0..pm::NPORTS { pm::set_protocol(p, pm::PROTO_OFF, None, false); }
            // AUX clock off again (and its IRQ): an unread stray byte from a
            // device we could not set up would block the keyboard.
            unsafe {
                kb::AUX_BUSY.store(true, Ordering::Release);
                let _ = kb::command(0xA7);
                let _ = ctr_update(0x20, 0x02);
                let _ = kb::flush();
                kb::AUX_BUSY.store(false, Ordering::Release);
            }
            log("[PSMOUSE] no PS/2 pointer: "); log(why); log("\n");
        }
    }
}

fn proto_name(proto: u8) -> &'static str {
    match proto {
        pm::PROTO_SYNAPTICS => "Synaptics absolute (tap, two-finger scroll)",
        pm::PROTO_EXPS => "IntelliMouse Explorer (wheel, 5 buttons)",
        pm::PROTO_IMPS => "ImPS/2 (wheel)",
        _ => "bare PS/2",
    }
}

/// One AUX loopback (0xD3) round trip, as Linux `I8042_CMD_AUX_LOOP`.
unsafe fn aux_loop(v: u8) -> Option<u8> {
    if !kb::command(0xD3) || !kb::wait_write() { return None; }
    outb(DATA, v);
    kb::wait_read()
}

/// Linux `i8042_set_mux_mode(true)`: F0, 56, A4 through the loopback; the
/// first two must echo, the third must not and is the MUX version.
unsafe fn check_mux() -> Option<u8> {
    let _ = kb::flush();
    let a = aux_loop(0xF0);
    let b = if a == Some(0xF0) { aux_loop(0x56) } else { None };
    let c = if b == Some(0x56) { aux_loop(0xA4) } else { None };
    log("[I8042] MUX loopback F0,56,A4 -> ");
    for v in [a, b, c] { match v { Some(x) => { hex(x as u32, 2); log(" "); } None => log("-- ") } }
    let _ = kb::flush();
    match c {
        Some(0xA4) | None => { log("(no active multiplexing)\n"); None }
        // USB legacy emulation interference fakes a v10.12 MUX (Linux).
        Some(0xAC) => { log("(0xAC: USB legacy emulation, ignored)\n"); None }
        Some(v) => { log("\n"); Some(v) }
    }
}

unsafe fn ctr_update(set: u8, clear: u8) -> Option<u8> {
    if !kb::command(0x20) { return None; }
    let ctr = kb::wait_read()?;
    let new = (ctr | set) & !clear;
    if !kb::command(0x60) || !kb::wait_write() { return None; }
    outb(DATA, new);
    Some(ctr)
}

unsafe fn setup(forced: Option<u8>) -> Result<(), &'static str> {
    PORT.store(pm::LEGACY as u8, Ordering::Relaxed);
    kb::MUX.store(false, Ordering::Release);
    let _ = kb::flush();
    // AUX IRQ off while polling; AUX clock on.
    let ctr = ctr_update(0, 0x02 | 0x20).ok_or("command byte timeout")?;
    log("[PSMOUSE] i8042 command byte 0x"); hex(ctr as u32, 2);
    if !kb::command(0xA8) { return Err("timeout enabling the AUX port"); }
    if !kb::command(0xA9) { return Err("timeout on the AUX test"); }
    match kb::wait_read() {
        Some(t) => { log(", AUX test 0x"); hex(t as u32, 2); log(if t == 0 { " (ok)\n" } else { " (fail, continuing)\n" }); }
        None => log(", AUX test: no reply (continuing)\n"),
    }
    let _ = kb::flush();

    let ver = if pm::nomux() { log("[I8042] i8042.nomux=1: legacy AUX port\n"); None } else { check_mux() };
    let Some(ver) = ver else {
        let proto = setup_port(forced)?;
        log("[PSMOUSE] ready: "); log(proto_name(proto)); log("\n");
        ctr_update(0x02, 0x20).ok_or("command byte timeout enabling the AUX IRQ")?;
        return Ok(());
    };
    log("[I8042] active multiplexing rev "); dec((ver >> 4) as u32); log("."); dec((ver & 15) as u32); log("\n");
    // Linux i8042_enable_mux_ports: 0x90+n then 0xA8 for every port.
    for n in 0..4u8 { let _ = kb::command(0x90 + n); let _ = kb::command(0xA8); }
    let _ = kb::flush();
    kb::MUX.store(true, Ordering::Release);
    // A forced Synaptics would apply to every port: let each auto-detect.
    let forced = forced.filter(|&p| p != pm::PROTO_SYNAPTICS);
    let mut protos = [pm::PROTO_OFF; 4];
    for n in 0..4u8 {
        PORT.store(n, Ordering::Relaxed);
        match setup_port(forced) {
            Ok(p) => { protos[n as usize] = p; log("[I8042] AUX"); dec(n as u32); log(": "); log(proto_name(p)); log("\n"); }
            Err(why) => { log("[I8042] AUX"); dec(n as u32); log(": nothing ("); log(why); log(")\n"); }
        }
    }
    PORT.store(pm::LEGACY as u8, Ordering::Relaxed);
    if protos.iter().all(|&p| p == pm::PROTO_OFF) { return Err("no device on any multiplexed AUX port"); }
    // Plain relative devices next to a touchpad are pointing sticks.
    if protos.contains(&pm::PROTO_SYNAPTICS) {
        for n in 0..4usize {
            if matches!(protos[n], pm::PROTO_BARE | pm::PROTO_IMPS) {
                pm::set_protocol(n, protos[n], None, true);
                log("[I8042] AUX"); dec(n as u32); log(": pointing stick (middle-button scrolling)\n");
            }
        }
    }
    let _ = kb::flush();
    ctr_update(0x02, 0x20).ok_or("command byte timeout enabling the AUX IRQ")?;
    Ok(())
}

/// Reset, identify and enable the device on the current `PORT`.
unsafe fn setup_port(forced: Option<u8>) -> Result<u8, &'static str> {
    let _ = kb::flush();
    let id = reset().ok_or("no reply to AUX reset (no PS/2 pointing device)")?;
    let _ = id;

    let mut proto = forced;
    let mut syn: Option<SynInfo> = None;
    let mut was_synaptics = false;
    if matches!(forced, None | Some(pm::PROTO_SYNAPTICS)) {
        syn = synaptics_probe(&mut was_synaptics, forced == Some(pm::PROTO_SYNAPTICS));
        if syn.is_some() { proto = Some(pm::PROTO_SYNAPTICS); }
        else if forced == Some(pm::PROTO_SYNAPTICS) {
            log("[PSMOUSE] psmouse.proto=synaptics but setup failed: falling back to relative\n");
            proto = None;
        }
        if syn.is_none() {
            // Leave whatever half-set state the probe caused.
            let _ = reset();
        }
    }
    if proto != Some(pm::PROTO_SYNAPTICS) {
        let p = match proto {
            Some(pm::PROTO_BARE) => pm::PROTO_BARE,
            Some(pm::PROTO_IMPS) => if knock(200, 100, 80) == Some(3) { pm::PROTO_IMPS } else { pm::PROTO_BARE },
            _ => {
                let mut p = pm::PROTO_BARE;
                let mut im = knock(200, 100, 80);
                log("[PSMOUSE] ImPS/2 knock ID 0x"); hex(im.unwrap_or(0xFF) as u32, 2); log("\n");
                if im != Some(3) { let _ = kb::flush(); im = knock(200, 100, 80); }
                if im == Some(3) {
                    p = pm::PROTO_IMPS;
                    if proto != Some(pm::PROTO_IMPS) && knock(200, 200, 80) == Some(4) { p = pm::PROTO_EXPS; }
                }
                p
            }
        };
        log("[PSMOUSE] relative protocol: ID 0x"); hex(get_id().unwrap_or(0xFF) as u32, 2);
        log(match p { pm::PROTO_EXPS => " -> exps\n", pm::PROTO_IMPS => " -> imps\n", _ => " -> bare\n" });
        let _ = aux_cmd(0xF3, Some(100), &mut []);
        // 8 counts/mm for a mouse; a Synaptics pad left in relative mode
        // streams 80 packets/s, so give it 4/mm (one count = one pixel).
        let _ = aux_cmd(0xE8, Some(if was_synaptics { 2 } else { 3 }), &mut []);
        let _ = aux_cmd(0xE6, None, &mut []);
        proto = Some(p);
    }
    let proto = proto.unwrap_or(pm::PROTO_BARE);
    pm::set_protocol(port() as usize, proto, syn, false);
    if aux_send(0xF4).is_err() {
        pm::set_protocol(port() as usize, pm::PROTO_OFF, None, false);
        return Err("device did not ACK enable (0xF4)");
    }
    let _ = kb::flush();
    Ok(proto)
}

/// Identify a Synaptics pad, log everything, switch it to absolute W mode.
///
/// The identify is tried three ways before giving up, each logged raw: the
/// E6-prefixed sliced form, Linux's exact `synaptics_detect` form (four
/// SETRES 0x00 then GETINFO, no E6), and the sliced form again after a fresh
/// reset. Boot 6 on the EliteBook 840 G5 (ACPI says SYNA3070/PNP0F13) got
/// `00 46 12` once, which is one bit off the 0x47 magic and has major
/// version 2 where a Synaptics pad of that age reports 8, so a corrupted or
/// shifted reply is as likely as a foreign vendor. `force` is
/// `psmouse.proto=synaptics`: proceed on a bad magic and let the
/// capability query decide.
unsafe fn synaptics_probe(found: &mut bool, force: bool) -> Option<SynInfo> {
    let mut r = [0u8; 3];
    let mut ok = false;
    for attempt in 0..3u32 {
        let _ = kb::flush();
        let idr = match attempt {
            1 => {
                let mut v = [0u8; 3];
                let mut good = true;
                for _ in 0..4 { if !aux_cmd(0xE8, Some(0), &mut []) { good = false; break; } }
                if good && aux_cmd(0xE9, None, &mut v) { Some(v) } else { None }
            }
            2 => { if reset().is_none() { None } else { syn_query(0x00) } }
            _ => syn_query(0x00),
        };
        log("[PSMOUSE] synaptics identify#"); dec(attempt); log(": ");
        match idr { Some(v) => log3(&v), None => log("no reply") }
        if let Some(v) = idr {
            r = v;
            if v[1] == 0x47 { log("\n"); ok = true; break; }
        }
        log(" (not Synaptics)\n");
    }
    if !ok && !force { return None; }
    if !ok { log("[PSMOUSE] psmouse.proto=synaptics: ignoring the identify magic\n"); }
    *found = true;
    log(" -> Synaptics v"); dec((r[2] & 0x0F) as u32); log("."); dec(r[0] as u32);
    log(" model code "); dec((r[2] >> 4) as u32); log("\n");

    let q = |name: &str, n: u8| -> Option<u32> {
        let r = syn_query(n);
        log("[PSMOUSE]   "); log(name); log(" (0x"); hex(n as u32, 2); log("): ");
        match r { Some(r) => { log3(&r); log("\n"); Some((r[0] as u32) << 16 | (r[1] as u32) << 8 | r[2] as u32) } None => { log("no reply\n"); None } }
    };
    let _model = q("model id", 0x01);
    let caps = q("capabilities", 0x02).unwrap_or(0);
    let _modes = q("modes", 0x03);
    if (caps >> 8) & 0xFF != 0x47 && !force { log("[PSMOUSE] capabilities invalid: staying relative\n"); return None; }
    let extended = caps & (1 << 23) != 0;
    let n_ext = (caps >> 20) & 7;
    let passthrough = caps & (1 << 7) != 0;
    let multifinger = caps & (1 << 1) != 0;
    let middle = caps & (1 << 18) != 0;
    log("[PSMOUSE]   caps: extended="); dec(extended as u32);
    log(" ext_queries="); dec(n_ext);
    log(" middle_btn="); dec(middle as u32);
    log(" passthrough="); dec(passthrough as u32);
    log(" multifinger="); dec(multifinger as u32);
    log(" palmdetect="); dec((caps & 1) as u32);
    log(" four_btn="); dec(((caps >> 3) & 1) as u32);
    log("\n");
    let mut ext09 = 0;
    let mut ext0c = 0;
    if n_ext >= 1 { ext09 = q("extended caps", 0x09).unwrap_or(0); }
    if n_ext >= 2 { let _ = q("firmware id", 0x0A); }
    if n_ext >= 4 { ext0c = q("extended caps 0c", 0x0C).unwrap_or(0); }
    let mut s = SynInfo {
        x_min: 1472, x_max: 5472, y_min: 1408, y_max: 4448,
        multifinger, middle_button: middle, passthrough,
        clickpad: ext0c & 0x100000 != 0,
    };
    log("[PSMOUSE]   ext: adv_gesture="); dec((ext09 >> 19 & 1) as u32);
    log(" image_sensor="); dec((ext09 >> 11 & 1) as u32);
    log(" multi_btn_no="); dec((ext09 >> 12) & 0xF);
    log(" clickpad="); dec(s.clickpad as u32);
    log(" clickpad2btn="); dec((ext0c >> 8 & 1) as u32);
    log(" intertouch(SMBus)="); dec((ext0c >> 14 & 1) as u32);
    log("\n");
    let coords = |r: u32| -> (i32, i32) {
        let (a, b, c) = ((r >> 16) as i32 & 0xFF, (r >> 8) as i32 & 0xFF, r as i32 & 0xFF);
        ((a << 5) | ((b & 0x0F) << 1), (c << 5) | ((b & 0xF0) >> 3))
    };
    if n_ext >= 5 && ext0c & 0x020000 != 0 {
        if let Some(r) = q("max coords", 0x0D) { let (x, y) = coords(r); if x > 0 && y > 0 { s.x_max = x; s.y_max = y; } }
    }
    if n_ext >= 7 && ext0c & 0x002000 != 0 {
        if let Some(r) = q("min coords", 0x0F) { let (x, y) = coords(r); if x > 0 && y > 0 { s.x_min = x; s.y_min = y; } }
    }
    log("[PSMOUSE]   range x "); dec(s.x_min as u32); log(".."); dec(s.x_max as u32);
    log(" y "); dec(s.y_min as u32); log(".."); dec(s.y_max as u32); log("\n");
    if s.x_max <= s.x_min + 500 || s.y_max <= s.y_min + 500 {
        log("[PSMOUSE] implausible range: staying relative\n");
        return None;
    }
    if !extended {
        log("[PSMOUSE] no extended capability (no W mode): staying relative\n");
        return None;
    }

    // Absolute | high rate (80/s) | disable gestures | W mode.
    const MODE: u8 = 0x80 | 0x40 | 0x04 | 0x01;
    if !(sliced(MODE) && aux_cmd(0xF3, Some(0x14), &mut [])) {
        log("[PSMOUSE] set mode failed: staying relative\n");
        return None;
    }
    let m = syn_query(0x03);
    log("[PSMOUSE]   mode set 0x"); hex(MODE as u32, 2); log(", modes now: ");
    match m { Some(r) => log3(&r), None => log("no reply") }
    log("\n");
    if let Some(r) = m {
        if r[2] & 0x80 == 0 {
            log("[PSMOUSE] absolute mode bit not set: staying relative\n");
            return None;
        }
    }
    if passthrough {
        // Enable the guest (pointing stick): a pass-through write is the
        // sliced byte followed by SET_RATE 0x28.
        let ok = sliced(0xF4) && aux_cmd(0xF3, Some(0x28), &mut []);
        log("[PSMOUSE]   pass-through device enable: ");
        log(if ok { "sent\n" } else { "FAILED (stick may stay silent)\n" });
        // Its ACK comes back as a W=3 packet; drop it here.
        let _ = aux_read(1);
        let _ = kb::flush();
    }
    Some(s)
}

/// Log ACPI devices that hint at the touchpad's bus: PNP0C50 (I2C HID),
/// SYN*/ELAN* hardware IDs, PNP0F13 (PS/2 mouse). Raw AML string scan of the
/// DSDT and SSDTs; capped.
pub fn scan_acpi(rsdp_phys: u64, hhdm_offset: u64) {
    let mut hits = 0u32;
    let mut cb = |aml: &[u8]| {
        let sig = core::str::from_utf8(&aml[..4]).unwrap_or("????");
        let pats: [&[u8]; 6] = [b"PNP0C50", b"ACPI0C50", b"SYN", b"ELAN", b"PNP0F13", b"HPQ"];
        // EISA-encoded PNP0C50 / PNP0F13 (AML EisaId → DWordConst 0x0C).
        let eisa: [(&[u8], &str); 2] = [(&[0x0C, 0x41, 0xD0, 0x0C, 0x50], "EisaId(PNP0C50)"), (&[0x0C, 0x41, 0xD0, 0x0F, 0x13], "EisaId(PNP0F13)")];
        let mut i = 36;
        while i < aml.len() && hits < 40 {
            let rest = &aml[i..];
            let mut hit = None;
            for p in pats.iter() {
                if rest.starts_with(p) && i > 0 && aml[i - 1] == 0x0D { hit = Some(i); break; }
            }
            if let Some(start) = hit {
                let end = aml[start..].iter().position(|&c| c == 0).map_or(aml.len(), |n| start + n.min(16));
                log("[PSMOUSE] ACPI "); log(sig); log(" +0x"); hex(start as u32, 5); log(" \"");
                for &c in &aml[start..end] { if (0x20..0x7F).contains(&c) { log(core::str::from_utf8(&[c]).unwrap_or("?")); } }
                log("\""); near_name(aml, start); log("\n");
                hits += 1;
                i = end;
                continue;
            }
            for (p, name) in eisa.iter() {
                if rest.starts_with(p) {
                    log("[PSMOUSE] ACPI "); log(sig); log(" +0x"); hex(i as u32, 5); log(" "); log(name);
                    near_name(aml, i); log("\n");
                    hits += 1;
                }
            }
            i += 1;
        }
    };
    unsafe { boot::acpi::for_each_aml_table(rsdp_phys, hhdm_offset, &mut cb) };
    if hits == 0 { log("[PSMOUSE] ACPI: no PNP0C50/SYN/ELAN/PNP0F13/HPQ ids in DSDT/SSDTs\n"); }
}

/// The NameSeg just before an ID (`_HID`/`_CID` in `Name(_HID, ...)`), and
/// the nearest preceding `Device` (5B 82) name, for context.
fn near_name(aml: &[u8], at: usize) {
    let lo = at.saturating_sub(6);
    if let Some(p) = aml[lo..at].windows(4).rposition(|w| w[0] == b'_' && w.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')) {
        log(" in "); log(core::str::from_utf8(&aml[lo + p..lo + p + 4]).unwrap_or("?"));
    }
    let lo = at.saturating_sub(512);
    if let Some(p) = aml[lo..at].windows(2).rposition(|w| w == [0x5B, 0x82]) {
        // 5B 82 PkgLength NameString; PkgLength is 1..4 bytes.
        let q = lo + p + 2;
        let n = 1 + ((aml[q] >> 6) as usize);
        let s = q + n;
        if s + 4 <= at {
            log(" of Device ");
            for &c in &aml[s..s + 4] { if c.is_ascii_graphic() { log(core::str::from_utf8(&[c]).unwrap_or("?")); } }
        }
    }
}
