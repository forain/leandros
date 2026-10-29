//! kapi exports: netdev
use super::*;


pub(super) struct ModNetif { ops: KNetifOps, ctx: usize, name: &'static str }

/// Write-once slots (leaked boxes); a netif's `dev` is its slot index.
pub(super) static MOD_NETIF: [AtomicUsize; drivers::netdev::MAX_NETIF] = [const { AtomicUsize::new(0) }; drivers::netdev::MAX_NETIF];
pub(super) static MOD_NETIF_LOCK: spin::Mutex<()> = spin::Mutex::new(());

pub(super) fn mn(d: u8) -> &'static ModNetif { unsafe { &*(MOD_NETIF[d as usize].load(Ordering::Acquire) as *const ModNetif) } }

pub(super) static MOD_OPS: drivers::netdev::NetifOps = drivers::netdev::NetifOps {
    driver: |d| mn(d).name,
    send: |d, b| { let m = mn(d); (m.ops.send)(m.ctx, b.as_ptr(), b.len()) },
    tx_ready: |d| { let m = mn(d); (m.ops.tx_ready)(m.ctx) },
    recv: |d, b| {
        let m = mn(d);
        let n = (m.ops.recv)(m.ctx, b.as_mut_ptr(), b.len());
        if n < 0 { None } else { Some((n as usize).min(b.len())) }
    },
    rx_rearm: |d| { let m = mn(d); (m.ops.rx_rearm)(m.ctx) },
    rx_irq_driven: |d| { let m = mn(d); (m.ops.rx_irq_driven)(m.ctx) },
    mac: |d| {
        let m = mn(d);
        let mut a = [0u8; 6];
        if (m.ops.mac)(m.ctx, a.as_mut_ptr()) { Some(a) } else { None }
    },
    link: |d| {
        let m = mn(d);
        let (mut up, mut mbps, mut full) = (false, 0u32, false);
        (m.ops.link)(m.ctx, &mut up, &mut mbps, &mut full);
        (up, mbps, full)
    },
    link_gen: |d| { let m = mn(d); (m.ops.link_gen)(m.ctx) },
    info: |d, out| {
        let m = mn(d);
        let mut buf = alloc::vec![0u8; 2048];
        let n = (m.ops.info)(m.ctx, buf.as_mut_ptr(), buf.len()).min(buf.len());
        out.push_str(&String::from_utf8_lossy(&buf[..n]));
    },
};

/// Register a netif from a module, at any time. Returns the netif id, or -errno
/// (-22 bad arguments, -12 no free slot). The net daemon is kicked and builds the stack on its next pass.
#[no_mangle]
pub extern "C" fn kapi_netdev_register(owner: KmodId, kind: u32, prefix: *const u8, prefix_len: usize,
                                       ops: *const KNetifOps, ctx: usize) -> i32 {
    let pfx = match core::str::from_utf8(unsafe { bytes(prefix, prefix_len.min(8)) }) {
        Ok(s) if !s.is_empty() && s.len() <= 6 && s.bytes().all(|b| b.is_ascii_lowercase()) => s,
        _ => return -22,
    };
    if ops.is_null() { return -22; }
    let ops = unsafe { *ops };
    let mut nb = [0u8; 32];
    let n = (ops.driver_name)(ctx, nb.as_mut_ptr(), nb.len()).min(nb.len());
    let name: &'static str = Box::leak(String::from_utf8_lossy(&nb[..n]).into_owned().into_boxed_str());
    let slot = {
        let _g = MOD_NETIF_LOCK.lock();
        let Some(i) = MOD_NETIF.iter().position(|s| s.load(Ordering::Acquire) == 0) else { return -12 };
        MOD_NETIF[i].store(Box::into_raw(Box::new(ModNetif { ops, ctx, name })) as usize, Ordering::Release);
        i
    };
    let k = if kind == KNET_WIFI { drivers::netdev::Kind::Wifi } else { drivers::netdev::Kind::Wired };
    match drivers::netdev::register_late(k, pfx, &MOD_OPS, slot as u8) {
        Some(id) => {
            crate::kmod::registry::note_registration(owner, RegKind::NETDEV, id as u64);
            id as i32
        }
        None => -12,
    }
}

#[no_mangle]
pub extern "C" fn kapi_netdev_kick() { drivers::netdev::kick(); }
#[no_mangle]
pub extern "C" fn kapi_netdev_kick_from_irq() { drivers::netdev::kick_from_irq(); }
