//! LEANDROS: the path-MTU cache (RFC 1191). An ICMP "fragmentation needed"
//! records the next-hop MTU for the datagram's destination; senders ask for
//! it before they build a packet. Lock-free: a few slots of (address, MTU +
//! expiry), a racing update at worst loses one entry.

use core::sync::atomic::{AtomicU64, Ordering};

const SLOTS: usize = 32;
/// Linux net.ipv4.route.mtu_expires.
const EXPIRES_MS: u64 = 600_000;
/// Linux net.ipv4.route.min_pmtu.
pub const MIN_PMTU: u16 = 552;

static KEY: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// mtu in bits 0..16, expiry (ms) above.
static VAL: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

fn slot_of(dst: u32) -> usize { (dst.wrapping_mul(0x9E37_79B1) >> 27) as usize % SLOTS }

/// Record `mtu` for `dst` (host-order IPv4 address) at time `now_ms`.
pub fn update(dst: u32, mtu: u16, now_ms: u64) {
    let mtu = mtu.max(MIN_PMTU);
    let k = dst as u64 | (1 << 40);
    let i = slot_of(dst);
    // An existing, smaller MTU only ever grows by expiring.
    if KEY[i].load(Ordering::Relaxed) == k {
        let v = VAL[i].load(Ordering::Relaxed);
        if v >> 16 > now_ms && (v & 0xffff) as u16 <= mtu { return; }
    }
    VAL[i].store((now_ms + EXPIRES_MS) << 16 | mtu as u64, Ordering::Relaxed);
    KEY[i].store(k, Ordering::Relaxed);
}

/// The cached path MTU to `dst`, if any and not expired.
pub fn lookup(dst: u32, now_ms: u64) -> Option<u16> {
    let i = slot_of(dst);
    if KEY[i].load(Ordering::Relaxed) != dst as u64 | (1 << 40) { return None; }
    let v = VAL[i].load(Ordering::Relaxed);
    if v >> 16 <= now_ms { return None; }
    Some((v & 0xffff) as u16)
}

/// Forget everything (tests).
pub fn clear() { for k in KEY.iter() { k.store(0, Ordering::Relaxed); } }
