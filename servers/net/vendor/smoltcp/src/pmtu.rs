//! LEANDROS: the path-MTU cache (RFC 1191 / RFC 8201). An ICMP "fragmentation
//! needed" or ICMPv6 "packet too big" records the next-hop MTU for the
//! datagram's destination; senders ask for it before they build a packet.
//! Keyed by (scope, address): the scope is the stack (network namespace) the
//! error arrived on, so one namespace's path never leaks into another's.

use core::sync::atomic::{AtomicU64, Ordering};

const SLOTS: usize = 64;
/// Linux net.ipv4.route.mtu_expires.
const EXPIRES_MS: u64 = 600_000;
/// Linux net.ipv4.route.min_pmtu.
pub const MIN_PMTU: u16 = 552;
/// IPv6 minimum link MTU (RFC 8200).
pub const MIN_PMTU6: u16 = 1280;

/// 64-bit hash of (scope, address), never 0; scope in bits 48.. of SCOPE.
static KEY: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static SCOPE: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// mtu in bits 0..16, expiry (ms) above.
static VAL: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

fn hash(scope: u16, a: &[u8; 16]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ scope as u64;
    for b in a { h = (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3); }
    h = (h ^ (h >> 29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h | 1
}

pub fn v4(a: [u8; 4]) -> [u8; 16] {
    let mut r = [0u8; 16];
    r[10] = 0xff; r[11] = 0xff; r[12..].copy_from_slice(&a);
    r
}

/// Record `mtu` for `dst` in `scope` at time `now_ms`; `min` is the family's floor.
pub fn update(scope: u16, dst: [u8; 16], mtu: u16, min: u16, now_ms: u64) {
    let mtu = mtu.max(min);
    let k = hash(scope, &dst);
    let i = (k >> 8) as usize % SLOTS;
    // An existing, smaller MTU only ever grows by expiring.
    if KEY[i].load(Ordering::Relaxed) == k {
        let v = VAL[i].load(Ordering::Relaxed);
        if v >> 16 > now_ms && (v & 0xffff) as u16 <= mtu { return; }
    }
    VAL[i].store((now_ms + EXPIRES_MS) << 16 | mtu as u64, Ordering::Relaxed);
    SCOPE[i].store(scope as u64 + 1, Ordering::Relaxed);
    KEY[i].store(k, Ordering::Relaxed);
}

/// The cached path MTU to `dst` in `scope`, if any and not expired.
pub fn lookup(scope: u16, dst: [u8; 16], now_ms: u64) -> Option<u16> {
    let k = hash(scope, &dst);
    let i = (k >> 8) as usize % SLOTS;
    if KEY[i].load(Ordering::Relaxed) != k { return None; }
    let v = VAL[i].load(Ordering::Relaxed);
    if v >> 16 <= now_ms { return None; }
    Some((v & 0xffff) as u16)
}

/// Forget a scope's entries (its stack was torn down).
pub fn clear_scope(scope: u16) {
    for i in 0..SLOTS {
        if SCOPE[i].load(Ordering::Relaxed) == scope as u64 + 1 { KEY[i].store(0, Ordering::Relaxed); }
    }
}

/// Forget everything (tests).
pub fn clear() { for k in KEY.iter() { k.store(0, Ordering::Relaxed); } }
