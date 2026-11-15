//! LEANDROS: the path-MTU cache (RFC 1191 / RFC 8201). An ICMP "fragmentation
//! needed" or ICMPv6 "packet too big" records the next-hop MTU for the
//! datagram's destination; senders ask for it before they build a packet.
//! Keyed by (scope, address): the scope is the stack (network namespace) the
//! error arrived on, so one namespace's path never leaks into another's.

use core::sync::atomic::{AtomicBool, Ordering};

const SLOTS: usize = 64;
/// Linux net.ipv4.route.mtu_expires.
const EXPIRES_MS: u64 = 600_000;
/// Linux net.ipv4.route.min_pmtu.
pub const MIN_PMTU: u16 = 552;
/// IPv6 minimum link MTU (RFC 8200).
pub const MIN_PMTU6: u16 = 1280;

#[derive(Clone, Copy)]
struct Ent { scope: u16, addr: [u8; 16], mtu: u16, expiry: u64, used: bool }
const EMPTY: Ent = Ent { scope: 0, addr: [0; 16], mtu: 0, expiry: 0, used: false };

static LOCK: AtomicBool = AtomicBool::new(false);
static mut TAB: [Ent; SLOTS] = [EMPTY; SLOTS];

fn with<R>(f: impl FnOnce(&mut [Ent; SLOTS]) -> R) -> R {
    while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() { core::hint::spin_loop(); }
    // SAFETY: the spin lock above serialises every access.
    let r = f(unsafe { &mut *core::ptr::addr_of_mut!(TAB) });
    LOCK.store(false, Ordering::Release);
    r
}

fn slot_of(scope: u16, a: &[u8; 16]) -> usize {
    let mut h: u32 = scope as u32;
    for b in a { h = h.wrapping_mul(31).wrapping_add(*b as u32); }
    (h.wrapping_mul(0x9E37_79B1) >> 20) as usize % SLOTS
}

pub fn v4(a: [u8; 4]) -> [u8; 16] {
    let mut r = [0u8; 16];
    r[10] = 0xff; r[11] = 0xff; r[12..].copy_from_slice(&a);
    r
}

/// Record `mtu` for `dst` in `scope` at time `now_ms`; `min` is the family's floor.
pub fn update(scope: u16, dst: [u8; 16], mtu: u16, min: u16, now_ms: u64) {
    let mtu = mtu.max(min);
    let i = slot_of(scope, &dst);
    with(|t| {
        let e = &mut t[i];
        // An existing, smaller MTU only ever grows by expiring.
        if e.used && e.scope == scope && e.addr == dst && e.expiry > now_ms && e.mtu <= mtu { return; }
        *e = Ent { scope, addr: dst, mtu, expiry: now_ms + EXPIRES_MS, used: true };
    });
}

/// The cached path MTU to `dst` in `scope`, if any and not expired.
pub fn lookup(scope: u16, dst: [u8; 16], now_ms: u64) -> Option<u16> {
    let i = slot_of(scope, &dst);
    with(|t| {
        let e = &t[i];
        if e.used && e.scope == scope && e.addr == dst && e.expiry > now_ms { Some(e.mtu) } else { None }
    })
}

/// Forget a scope's entries (its stack was torn down).
pub fn clear_scope(scope: u16) { with(|t| for e in t.iter_mut() { if e.scope == scope { e.used = false; } }) }

/// Forget everything (tests).
pub fn clear() { with(|t| for e in t.iter_mut() { e.used = false; }) }
