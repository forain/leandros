//! kapi exports: netdev
use super::*;


use crate::kmod::gate::{Class, Gate};
use core::sync::atomic::AtomicU32;

pub(super) struct ModNetif {
    ops: KNetifOps, ctx: usize, name: &'static str,
    gate: Arc<Gate>,
    mac: Option<[u8; 6]>,
    /// Added to the module's link generation so a reload always looks like a new link-up.
    genbase: u32,
    last_gen: AtomicU32,
}

/// Write-once-per-load slots (leaked boxes); a netif's `dev` is its slot index. A slot is only
/// overwritten when the same netif is rebound by the next load (the old box stays leaked: its gate is closed).
pub(super) static MOD_NETIF: [AtomicUsize; drivers::netdev::MAX_NETIF] = [const { AtomicUsize::new(0) }; drivers::netdev::MAX_NETIF];
pub(super) static MOD_NETIF_LOCK: spin::Mutex<()> = spin::Mutex::new(());

pub(super) fn mn(d: u8) -> &'static ModNetif { unsafe { &*(MOD_NETIF[d as usize].load(Ordering::Acquire) as *const ModNetif) } }

/// Netifs whose module was unloaded: (netif id, slot, last mac, last link generation).
static PENDING: spin::Mutex<alloc::vec::Vec<(u8, usize, Option<[u8; 6]>, u32)>> = spin::Mutex::new(alloc::vec::Vec::new());
static DETACHED: spin::Mutex<alloc::vec::Vec<(u8, usize, Option<[u8; 6]>, u32)>> = spin::Mutex::new(alloc::vec::Vec::new());

pub(super) static MOD_OPS: drivers::netdev::NetifOps = drivers::netdev::NetifOps {
    driver: |d| mn(d).name,
    send: |d, b| { let m = mn(d); m.gate.call(Class::Upper, || (m.ops.send)(m.ctx, b.as_ptr(), b.len())).unwrap_or(false) },
    tx_ready: |d| { let m = mn(d); m.gate.call(Class::Upper, || (m.ops.tx_ready)(m.ctx)).unwrap_or(false) },
    recv: |d, b| {
        let m = mn(d);
        let n = m.gate.call(Class::Upper, || (m.ops.recv)(m.ctx, b.as_mut_ptr(), b.len())).unwrap_or(-1);
        if n < 0 { None } else { Some((n as usize).min(b.len())) }
    },
    rx_rearm: |d| { let m = mn(d); m.gate.call(Class::Upper, || (m.ops.rx_rearm)(m.ctx)).unwrap_or(false) },
    rx_irq_driven: |d| { let m = mn(d); m.gate.call(Class::Upper, || (m.ops.rx_irq_driven)(m.ctx)).unwrap_or(false) },
    mac: |d| {
        let m = mn(d);
        let mut a = [0u8; 6];
        match m.gate.call(Class::Upper, || (m.ops.mac)(m.ctx, a.as_mut_ptr())) {
            Some(true) => Some(a),
            Some(false) => None,
            None => m.mac,
        }
    },
    link: |d| {
        let m = mn(d);
        let (mut up, mut mbps, mut full) = (false, 0u32, false);
        m.gate.call(Class::Upper, || (m.ops.link)(m.ctx, &mut up, &mut mbps, &mut full));
        (up, mbps, full)
    },
    link_gen: |d| {
        let m = mn(d);
        match m.gate.call(Class::Upper, || (m.ops.link_gen)(m.ctx)) {
            Some(g) => { let g = g.wrapping_add(m.genbase); m.last_gen.store(g, Ordering::Relaxed); g }
            None => m.last_gen.load(Ordering::Relaxed),
        }
    },
    info: |d, out| {
        let m = mn(d);
        let mut buf = alloc::vec![0u8; 2048];
        let n = m.gate.call(Class::Upper, || (m.ops.info)(m.ctx, buf.as_mut_ptr(), buf.len())).unwrap_or(0).min(buf.len());
        out.push_str(&String::from_utf8_lossy(&buf[..n]));
    },
};

/// Unload step 2: the module's netifs stop calling into it (gate still open until the drain, so this
/// only records what a reload needs). The netif entry itself stays, keeping its name.
pub fn netdev_detach(id: u64) {
    let id = id as u8;
    let Some(n) = drivers::netdev::get(id as usize) else { return };
    let m = mn(n.dev);
    let mut p = PENDING.lock();
    p.retain(|x| x.0 != id);
    p.push((id, n.dev as usize, m.mac, m.last_gen.load(Ordering::Relaxed)));
}

/// Both drains succeeded: the netif may be rebound by a later load. Without this (Zombie) it stays bound to the zombie.
pub fn netdev_commit_detach(id: u64) {
    let id = id as u8;
    let mut p = PENDING.lock();
    if let Some(i) = p.iter().position(|x| x.0 == id) { let e = p.remove(i); DETACHED.lock().push(e); }
}

/// Unload failed before the commit: forget the pending record.
pub fn netdev_abort_detach(id: u64) { let id = id as u8; PENDING.lock().retain(|x| x.0 != id); }

/// Register a netif from a module, at any time. Returns the netif id, or -errno
/// (-22 bad arguments, -12 no free slot). The net daemon is kicked and builds the stack on its next pass.
/// A netif detached by an earlier unload (same prefix, kind and MAC) is rebound, keeping its name.
#[no_mangle]
pub extern "C" fn kapi_netdev_register(owner: KmodId, kind: u32, prefix: *const u8, prefix_len: usize,
                                       ops: *const KNetifOps, ctx: usize) -> i32 {
    let pfx = match core::str::from_utf8(unsafe { bytes(prefix, prefix_len.min(8)) }) {
        Ok(s) if !s.is_empty() && s.len() <= 6 && s.bytes().all(|b| b.is_ascii_lowercase()) => s,
        _ => return -22,
    };
    if ops.is_null() { return -22; }
    let ops = unsafe { *ops };
    let owner = crate::kmod::gate::owner(owner);
    let Some(gate) = crate::kmod::gate::get(owner) else { return -19 };
    let mut nb = [0u8; 32];
    let n = (ops.driver_name)(ctx, nb.as_mut_ptr(), nb.len()).min(nb.len());
    let name: &'static str = Box::leak(String::from_utf8_lossy(&nb[..n]).into_owned().into_boxed_str());
    let mut mb = [0u8; 6];
    let mac = if (ops.mac)(ctx, mb.as_mut_ptr()) { Some(mb) } else { None };
    let k = if kind == KNET_WIFI { drivers::netdev::Kind::Wifi } else { drivers::netdev::Kind::Wired };
    let mk = |genbase| Box::into_raw(Box::new(ModNetif { ops, ctx, name, gate: gate.clone(), mac, genbase, last_gen: AtomicU32::new(0) })) as usize;
    // Rebind a detached netif of this prefix and MAC (or the first of the prefix when the MAC is unknown).
    {
        let _g = MOD_NETIF_LOCK.lock();
        let mut det = DETACHED.lock();
        let pos = det.iter().position(|d| {
            let Some(nif) = drivers::netdev::get(d.0 as usize) else { return false };
            nif.kind == k && nif.name().starts_with(pfx) && (mac.is_none() || d.2.is_none() || d.2 == mac)
        });
        if let Some(p) = pos {
            let (id, slot, _, lastgen) = det.remove(p);
            MOD_NETIF[slot].store(mk(lastgen.wrapping_add(1)), Ordering::Release);
            drop(det);
            crate::kmod::registry::note_registration(owner, RegKind::NETDEV, id as u64);
            drivers::netdev::kick();
            return id as i32;
        }
    }
    let slot = {
        let _g = MOD_NETIF_LOCK.lock();
        let Some(i) = MOD_NETIF.iter().position(|s| s.load(Ordering::Acquire) == 0) else { return -12 };
        MOD_NETIF[i].store(mk(0), Ordering::Release);
        i
    };
    match drivers::netdev::register_late(k, pfx, &MOD_OPS, slot as u8) {
        Some(id) => {
            crate::kmod::registry::note_registration(owner, RegKind::NETDEV, id as u64);
            // A module Wi-Fi netif gets its netctl control side the same way the built-in one does.
            #[cfg(feature = "drv-wifi-stack")]
            if kind == KNET_WIFI {
                if let Some(n) = drivers::netdev::get(id as usize) { drivers::wifi_class::attach(name, n.ifindex(), n.name()); }
            }
            id as i32
        }
        None => -12,
    }
}

#[no_mangle]
pub extern "C" fn kapi_netdev_kick() { drivers::netdev::kick(); }
#[no_mangle]
pub extern "C" fn kapi_netdev_kick_from_irq() { drivers::netdev::kick_from_irq(); }
