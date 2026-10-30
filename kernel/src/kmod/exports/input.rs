//! Input class (kmod W2-6): a module claims one of the fixed evdev nodes (keyboard = event0,
//! pointer = event1) and feeds it through `kapi_input_event`, like the built-in virtio-input does.
use super::*;

const NODES: i32 = 2;

fn node_ok(node: i32) -> bool { (0..NODES).contains(&node) }

/// Claim the evdev node for `kind` (0 keyboard, 1 pointer). Returns the node, -22 (bad kind) or
/// -16 (already claimed).
#[no_mangle]
pub extern "C" fn kapi_input_register(owner: KmodId, kind: u32, _name: *const u8, _len: usize,
                                      _bus: u16, _vid: u16, _pid: u16) -> i32 {
    static CLAIMED: AtomicU32 = AtomicU32::new(0);
    if kind >= NODES as u32 { return -22; }
    if CLAIMED.fetch_or(1 << kind, Ordering::AcqRel) & (1 << kind) != 0 { return -16; }
    crate::kmod::registry::note_registration(owner, RegKind::INPUT, kind as u64);
    kind as i32
}

#[no_mangle]
pub extern "C" fn kapi_input_event(node: i32, ty: u16, code: u16, value: i32) {
    if node_ok(node) { evdev_server::push_event(node as u32, ty, code, value); }
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
