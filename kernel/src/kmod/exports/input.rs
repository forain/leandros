//! Input class (kmod W2-6): a module claims one of the fixed evdev nodes (keyboard = event0,
//! pointer = event1) and feeds it through `kapi_input_event`, like the built-in virtio-input does.
use super::*;

const NODES: i32 = 2;
static CLAIMED: AtomicU32 = AtomicU32::new(0);

/// Per-device nodes handed out by `kapi_input_register` for kinds `INPUT_DEV_KEYBOARD` / `INPUT_DEV_MOUSE`.
static DYN: spin::Mutex<alloc::vec::Vec<u32>> = spin::Mutex::new(alloc::vec::Vec::new());

fn node_ok(node: i32) -> bool {
    (0..NODES).contains(&node) || (node >= NODES && DYN.try_lock().map_or(false, |d| d.contains(&(node as u32))))
}

/// Claim the evdev node for `kind` (0 keyboard, 1 pointer). Returns the node, -22 (bad kind) or
/// -16 (already claimed).
#[no_mangle]
pub extern "C" fn kapi_input_register(owner: KmodId, kind: u32, _name: *const u8, _len: usize,
                                      _bus: u16, _vid: u16, _pid: u16) -> i32 {
    if kind == kmod_abi::input::INPUT_DEV_KEYBOARD || kind == kmod_abi::input::INPUT_DEV_MOUSE {
        // A hot-pluggable device with its own /dev/input/eventN (N >= 2).
        if _name.is_null() || _len > 80 { return -22; }
        let name = core::str::from_utf8(unsafe { core::slice::from_raw_parts(_name, _len) }).unwrap_or("input");
        let k = if kind == kmod_abi::input::INPUT_DEV_KEYBOARD { evdev_server::DEVKIND_KEYBOARD } else { evdev_server::DEVKIND_MOUSE };
        return match evdev_server::register_input_device(k, name, _bus, _vid, _pid) {
            Some(n) => { DYN.lock().push(n); crate::kmod::registry::note_registration(owner, RegKind::INPUT, 0x100 | n as u64); n as i32 }
            None => -12,
        };
    }
    if kind >= NODES as u32 { return -22; }
    if CLAIMED.fetch_or(1 << kind, Ordering::AcqRel) & (1 << kind) != 0 { return -16; }
    crate::kmod::registry::note_registration(owner, RegKind::INPUT, kind as u64);
    kind as i32
}

/// Currently pressed key/button codes of the fixed nodes (0 keyboard, 1 pointer), one bit per code below
/// 0x300. Lets `input_release` push the key-ups a vanishing source can no longer send.
const KEY_WORDS: usize = 12;
static PRESSED: [[AtomicU64; KEY_WORDS]; 2] = [const { [const { AtomicU64::new(0) }; KEY_WORDS] }; 2];

fn track_key(node: i32, ty: u16, code: u16, value: i32) {
    if ty != 1 || !(0..2).contains(&node) || code as usize >= KEY_WORDS * 64 { return; }
    let (w, b) = (code as usize / 64, 1u64 << (code % 64));
    if value == 0 { PRESSED[node as usize][w].fetch_and(!b, Ordering::AcqRel); }
    else { PRESSED[node as usize][w].fetch_or(b, Ordering::AcqRel); }
}

#[no_mangle]
pub extern "C" fn kapi_input_event(node: i32, ty: u16, code: u16, value: i32) {
    if node_ok(node) { track_key(node, ty, code, value); evdev_server::push_event(node as u32, ty, code, value); }
}

/// Publish a node's relative-axis and key/button bitmaps (EVIOCGBIT). 0, or -22.
#[no_mangle]
pub extern "C" fn kapi_input_set_caps(node: i32, rel: *const u8, rl: usize, key: *const u8, kl: usize) -> i32 {
    if !node_ok(node) || (rl != 0 && rel.is_null()) || (kl != 0 && key.is_null()) || rl > 64 || kl > 256 { return -22; }
    let r: &[u8] = if rl == 0 { &[] } else { unsafe { core::slice::from_raw_parts(rel, rl) } };
    let k: &[u8] = if kl == 0 { &[] } else { unsafe { core::slice::from_raw_parts(key, kl) } };
    evdev_server::set_device_caps(node as u32, r, k);
    0
}

/// Publish a node's INPUT_PROP_* and EV_FF bitmaps (EVIOCGPROP / EVIOCGBIT). 0, or -22.
#[no_mangle]
pub extern "C" fn kapi_input_set_props(node: i32, props: *const u8, pl: usize, ff: *const u8, fl: usize) -> i32 {
    if !node_ok(node) || (pl != 0 && props.is_null()) || (fl != 0 && ff.is_null()) || pl > 8 || fl > 16 { return -22; }
    let p: &[u8] = if pl == 0 { &[] } else { unsafe { core::slice::from_raw_parts(props, pl) } };
    let f: &[u8] = if fl == 0 { &[] } else { unsafe { core::slice::from_raw_parts(ff, fl) } };
    evdev_server::set_device_props(node as u32, p, f);
    0
}

/// Remove a per-device node made by `kapi_input_register`. 0, or -22 for a fixed or unknown node.
#[no_mangle]
pub extern "C" fn kapi_input_unregister(node: i32) -> i32 {
    if node < NODES { return -22; }
    let mut d = DYN.lock();
    let Some(i) = d.iter().position(|&n| n == node as u32) else { return -22 };
    d.remove(i);
    drop(d);
    evdev_server::unregister_input_device(node as u32);
    0
}

/// A key edge or SYN for the kernel's own console/VT keyboard (fixed node 0 only).
#[no_mangle]
pub extern "C" fn kapi_input_event_console(ty: u16, code: u16, value: i32) {
    track_key(0, ty, code, value);
    evdev_server::push_event_console(0, ty, code, value);
}

/// The keyboard LEDs to show, in HID output-report bit order (Num=1, Caps=2, Scroll=4): the
/// console's lock state while it owns the keyboard, else what the compositor last wrote.
#[no_mangle]
pub extern "C" fn kapi_input_leds() -> u32 { drivers::usb_hid::wanted_leds() as u32 }

/// The console keyboard repeat delay and period, in ms.
#[no_mangle]
pub extern "C" fn kapi_input_repeat(delay: *mut u32, rate: *mut u32) {
    let (d, r) = tty_server::keyboard::repeat_params();
    if !delay.is_null() { unsafe { *delay = d; } }
    if !rate.is_null() { unsafe { *rate = r; } }
}

/// Relative pointer motion/wheel/buttons from a mouse source, integrated into the absolute pointer node.
#[no_mangle]
pub extern "C" fn kapi_rel_pointer_report(src: u32, dx: i32, dy: i32, wheel: i32, hwheel: i32, buttons: u8) {
    drivers::rel_pointer::report(src as usize, dx, dy, wheel, hwheel, buttons);
}

/// Size the motion scale to the boot framebuffer and advertise the source's buttons/wheel axes.
#[no_mangle]
pub extern "C" fn kapi_rel_pointer_advertise(buttons: u8, hwheel: bool, hires: bool) {
    drivers::rel_pointer::init_scale();
    drivers::rel_pointer::advertise(buttons, hwheel, hires);
}

/// Unload: take a registration back (per-device node, or a fixed node's claim bit).
/// Per-device nodes are unregistered; a fixed node's claim bit is dropped (unless kmod.leakinput=1), after
/// key-up edges for everything still held so the console/compositor never sees a stuck key or modifier.
pub fn input_release(handle: u64) -> bool {
    if handle & 0x100 != 0 {
        let n = (handle & 0xff) as i32;
        let _ = kapi_input_unregister(n);
        true
    } else {
        // Fixed node (0 keyboard, 1 pointer): drop the claim so a reload can take it again. The console and
        // compositor only consume the node's events, so they just see the stream stop. kmod.leakinput=1 keeps
        // the old irrevocable behaviour for the kmbad_leak Zombie fixture.
        if crate::kmod::autoload::opt("kmod.leakinput") == Some("1") { return false; }
        let node = (handle & 0x1f) as usize;
        if node < 2 {
            // Key-up for every code still held, then SYN, so no key or modifier stays stuck.
            for w in 0..KEY_WORDS {
                let mut bits = PRESSED[node][w].swap(0, Ordering::AcqRel);
                while bits != 0 {
                    let code = (w * 64) as u16 + bits.trailing_zeros() as u16;
                    bits &= bits - 1;
                    evdev_server::push_event(node as u32, 1, code, 0);
                    if node == 0 { evdev_server::push_event_console(0, 1, code, 0); }
                }
            }
            evdev_server::push_event(node as u32, 0, 0, 0);
            if node == 0 { evdev_server::push_event_console(0, 0, 0, 0); }
        }
        CLAIMED.fetch_and(!(1u32 << (handle & 0x1f)), Ordering::AcqRel);
        true
    }
}
