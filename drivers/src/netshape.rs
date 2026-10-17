//! A link emulator for one netif (test knob, off unless the kernel command
//! line asks): `netshape=<ifname>:<tx_kbit>:<rx_kbit>[:<txq>[:<rxq>[:<delay_ms>[:<loss_permille>]]]]`.
//!
//! It puts a slow, queued link between the net server and the driver, the
//! shape of a Wi-Fi association: frames leave at `tx_kbit` and arrive at
//! `rx_kbit`, each direction a FIFO. The TX side holds the stack at `txq`
//! queued frames (the driver's `tx_ready` backpressure, iwlwifi's 64) and
//! drops beyond `txq + 32`; the RX side is the AP's queue for this station,
//! `rxq` frames, tail drop. `delay_ms` is added one way, to both directions;
//! `loss_permille` drops that share of frames at random, both directions
//! (air loss the retries did not recover).
//! Defaults: txq 64, rxq 256, delay 2 ms. A rate of 0 leaves that direction
//! unshaped. QEMU uses it to reproduce in a VM what the laptop's 8265 does to
//! TCP (lane wifirx); `/proc/net/nic` shows its counters (`shape:` line).
//!
//! Frames are released by the net daemon's own calls (send / recv / rearm);
//! [`next_due_ns`] tells it when the next one falls due so it does not
//! oversleep a release.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use spin::Mutex;

use crate::netdev::Netif;

/// Registry id of the shaped netif (0xff: none).
fn log(msg: &str) {
    extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }
    let mut s = alloc::string::String::with_capacity(msg.len() + 16);
    s.push_str("[NET] netshape: ");
    s.push_str(msg);
    s.push('\n');
    // SAFETY: the kernel's raw serial writer takes a pointer and length.
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

static TARGET: AtomicU8 = AtomicU8::new(0xff);
/// The next release instant (`monotonic_ns`), 0 when nothing is queued.
static NEXT_DUE: AtomicU64 = AtomicU64::new(0);

struct Dir {
    kbit: u64,
    /// When the link is free for the next frame.
    free_ns: u64,
    q: VecDeque<(u64, Vec<u8>)>,
    passed: u64,
    dropped: u64,
    lost: u64,
    max_depth: usize,
}

impl Dir {
    const fn new() -> Dir { Dir { kbit: 0, free_ns: 0, q: VecDeque::new(), passed: 0, dropped: 0, lost: 0, max_depth: 0 } }

    /// Schedule `f`: serialisation at the link rate after the frames ahead
    /// of it, then the propagation delay.
    fn push(&mut self, now: u64, delay_ns: u64, f: Vec<u8>) {
        let start = self.free_ns.max(now);
        // Bits on the air: the frame plus about 50 bytes of PHY/MAC overhead.
        self.free_ns = start + ((f.len() as u64 + 50) * 8 * 1_000_000) / self.kbit.max(1);
        self.q.push_back((self.free_ns + delay_ns, f));
        self.max_depth = self.max_depth.max(self.q.len());
    }

    fn pop_due(&mut self, now: u64) -> Option<Vec<u8>> {
        if self.q.front().map_or(false, |(t, _)| *t <= now) {
            self.passed += 1;
            return self.q.pop_front().map(|(_, f)| f);
        }
        None
    }
}

struct Shaper {
    nif: Option<Netif>,
    txq: usize,
    rxq: usize,
    delay_ns: u64,
    loss: u32,
    rng: u32,
    tx: Dir,
    rx: Dir,
    held: bool,
}

static SHAPER: Mutex<Shaper> = Mutex::new(Shaper {
    nif: None, txq: 64, rxq: 256, delay_ns: 2_000_000, loss: 0, rng: 0x2545_f491, tx: Dir::new(), rx: Dir::new(), held: false,
});

/// Parse the knob (kernel init, after the netifs registered).
pub fn init(cmdline: &str) {
    let Some(spec) = cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix("netshape=")).last() else { return };
    let mut it = spec.split(':');
    let name = it.next().unwrap_or("");
    let Some(nif) = crate::netdev::by_name(name) else {
        log(&alloc::format!("no netif '{}'", name));
        return;
    };
    let mut num = || it.next().and_then(|v| v.parse::<u64>().ok());
    let (txk, rxk) = (num().unwrap_or(0), num().unwrap_or(0));
    let mut s = SHAPER.lock();
    s.tx.kbit = txk;
    s.rx.kbit = rxk;
    if let Some(v) = num() { s.txq = (v as usize).max(1); }
    if let Some(v) = num() { s.rxq = (v as usize).max(1); }
    if let Some(v) = num() { s.delay_ns = v * 1_000_000; }
    if let Some(v) = num() { s.loss = v.min(1000) as u32; }
    s.nif = Some(nif);
    log(&alloc::format!("{} tx {} kbit/s rx {} kbit/s, tx queue {} (hold) / {} (drop), rx queue {}, delay {} ms, loss {}/1000",
                 name, txk, rxk, s.txq, s.txq + 32, s.rxq, s.delay_ns / 1_000_000, s.loss));
    TARGET.store(nif.id, Ordering::Release);
}

/// This netif's data path goes through the shaper.
#[inline]
pub fn shaped(id: u8) -> bool { TARGET.load(Ordering::Relaxed) == id }

/// Random loss: true drops this frame (xorshift32).
fn lose(s: &mut Shaper) -> bool {
    if s.loss == 0 { return false; }
    let mut x = s.rng;
    x ^= x << 13; x ^= x >> 17; x ^= x << 5;
    s.rng = x;
    x % 1000 < s.loss
}

fn update_due(s: &Shaper) {
    let t = [s.tx.q.front(), s.rx.q.front()].iter().flatten().map(|(t, _)| *t).min().unwrap_or(0);
    NEXT_DUE.store(t, Ordering::Relaxed);
}

/// Hand every due TX frame to the driver.
fn pump_tx(s: &mut Shaper, nif: &Netif, now: u64) {
    while let Some(f) = s.tx.pop_due(now) {
        (nif.ops.send)(nif.dev, &f);
    }
    if s.held && s.tx.q.len() < s.txq / 2 {
        s.held = false;
        crate::netdev::kick();
    }
}

pub fn send(nif: &Netif, buf: &[u8]) -> bool {
    let mut s = SHAPER.lock();
    if s.tx.kbit == 0 { drop(s); return (nif.ops.send)(nif.dev, buf); }
    let now = sched::monotonic_ns();
    pump_tx(&mut s, nif, now);
    if s.tx.q.len() >= s.txq + 32 { s.tx.dropped += 1; return false; }
    let d = s.delay_ns;
    if lose(&mut s) { s.tx.lost += 1; return true; }
    s.tx.push(now, d, buf.to_vec());
    update_due(&s);
    true
}

pub fn tx_ready(nif: &Netif) -> bool {
    let mut s = SHAPER.lock();
    if s.tx.kbit == 0 { drop(s); return (nif.ops.tx_ready)(nif.dev); }
    pump_tx(&mut s, nif, sched::monotonic_ns());
    if s.tx.q.len() < s.txq { return true; }
    s.held = true;
    false
}

pub fn recv(nif: &Netif, buf: &mut [u8]) -> Option<usize> {
    let mut s = SHAPER.lock();
    if s.rx.kbit == 0 { drop(s); return (nif.ops.recv)(nif.dev, buf); }
    let now = sched::monotonic_ns();
    pump_tx(&mut s, nif, now);
    // Everything the driver has goes onto the "air" queue first.
    let mut tmp = [0u8; 2048];
    while let Some(n) = (nif.ops.recv)(nif.dev, &mut tmp) {
        if s.rx.q.len() >= s.rxq { s.rx.dropped += 1; continue; }
        let d = s.delay_ns;
        if lose(&mut s) { s.rx.lost += 1; continue; }
        s.rx.push(now, d, tmp[..n].to_vec());
    }
    let f = s.rx.pop_due(now);
    update_due(&s);
    let f = f?;
    let n = f.len().min(buf.len());
    buf[..n].copy_from_slice(&f[..n]);
    Some(n)
}

pub fn rx_rearm(nif: &Netif) -> bool {
    let mut s = SHAPER.lock();
    let now = sched::monotonic_ns();
    if s.tx.kbit != 0 { pump_tx(&mut s, nif, now); }
    let more = (nif.ops.rx_rearm)(nif.dev);
    let due = s.rx.q.front().map_or(false, |(t, _)| *t <= now);
    update_due(&s);
    more || due
}

/// When the next queued frame falls due (`monotonic_ns`), if any.
pub fn next_due_ns() -> Option<u64> {
    if TARGET.load(Ordering::Relaxed) == 0xff { return None; }
    match NEXT_DUE.load(Ordering::Relaxed) { 0 => None, t => Some(t) }
}

/// `shape:` line for /proc/net/nic.
pub fn info_line(id: u8, out: &mut alloc::string::String) {
    use core::fmt::Write;
    if !shaped(id) { return; }
    let s = SHAPER.lock();
    let _ = writeln!(out, "shape: tx {} kbit/s passed {} dropped {} lost {} max_depth {} queued {}; rx {} kbit/s passed {} dropped {} lost {} max_depth {} queued {}; delay {} ms loss {}/1000",
                     s.tx.kbit, s.tx.passed, s.tx.dropped, s.tx.lost, s.tx.max_depth, s.tx.q.len(),
                     s.rx.kbit, s.rx.passed, s.rx.dropped, s.rx.lost, s.rx.max_depth, s.rx.q.len(), s.delay_ns / 1_000_000, s.loss);
}
