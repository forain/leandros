//! Wildcard sockets on a stack created after them. Pure logic (no kernel
//! types), compiled into the host tests in `servers/net/host-tests` via
//! `#[path]`.
//!
//! Background (lane pisshrace, 2026-10-27): `listen.rs` and `udpany.rs` put
//! an INADDR_ANY listener / UDP server on every stack that exists when the
//! socket is set up, on the premise that every NIC stack exists from boot.
//! Since lane pirxwedge that is false for Wi-Fi: brcmf learns its MAC from
//! the firmware seconds into boot, and the daemon builds wifi0's stack then.
//! Whether sshd's `listen(0.0.0.0:22)` ran before or after that point was a
//! boot-timeline race: before, wifi0 had no LISTEN socket and every SYN over
//! Wi-Fi got an RST ("Connection refused") while ping worked. Changing the
//! boot timeline (Bluetooth bring-up, RP1/PCIe probing) flipped the race on
//! the real Pi 5.
//!
//! Now each such socket is recorded in a [`Registry`] before it is put on
//! the existing stacks, and a stack created later [`adopt`]s every entry it
//! should carry. Races: a listen() that runs concurrently with the stack's
//! creation either sees the new stack (and creates there itself; `create`
//! is idempotent per key) or registered early enough for `adopt`'s snapshot;
//! a close() that runs concurrently unregisters first, so `adopt` notices
//! after its create and removes what it just added.

use alloc::vec::Vec;

/// Live wildcard sockets: (key, what to recreate them from).
pub struct Registry<E: Copy> {
    v: Vec<(u32, E)>,
}

impl<E: Copy> Registry<E> {
    pub const fn new() -> Self { Self { v: Vec::new() } }
    pub fn register(&mut self, key: u32, e: E) {
        if !self.contains(key) { self.v.push((key, e)); }
    }
    pub fn unregister(&mut self, key: u32) { self.v.retain(|x| x.0 != key); }
    pub fn contains(&self, key: u32) -> bool { self.v.iter().any(|x| x.0 == key) }
    pub fn snapshot(&self) -> Vec<(u32, E)> { self.v.clone() }
    pub fn len(&self) -> usize { self.v.len() }
}

/// Put every entry of `snap` that `wants` on the new stack (`create`,
/// idempotent, true when the socket is there afterwards). An entry that
/// was unregistered meanwhile (`still` false after the create) is taken off
/// again (`remove`). Returns how many entries the stack now carries.
pub fn adopt<E: Copy>(
    snap: &[(u32, E)],
    wants: impl Fn(&E) -> bool,
    mut create: impl FnMut(u32, &E) -> bool,
    still: impl Fn(u32) -> bool,
    mut remove: impl FnMut(u32),
) -> usize {
    let mut n = 0;
    for (key, e) in snap {
        if !wants(e) || !create(*key, e) { continue; }
        if still(*key) { n += 1; } else { remove(*key); }
    }
    n
}
