//! Kernel random number generator: getrandom(2), /dev/urandom, /dev/random.
//!
//! A ChaCha20 CSPRNG with fast key erasure (Bernstein, "Fast-key-erasure
//! random-number generators", 2017): each request runs ChaCha20 under the
//! current 256-bit key, hands out the stream after its first 32 bytes, and
//! those first 32 bytes become the next key. A captured key therefore
//! reveals nothing about output that was already returned.
//!
//! Seeding. `init()` runs once during boot, before any user process exists,
//! and hashes (BLAKE2s-256) everything it can gather into the first key:
//!   * the CPU's hardware generator, when it has one and says so —
//!     x86_64: RDSEED (CPUID.7.0:EBX[18]), else RDRAND (CPUID.1:ECX[30]),
//!     each with the architected retry loops; aarch64: RNDRRS, else RNDR
//!     (ID_AA64ISAR0_EL1.RNDR >= 1), whose failure is reported in NZCV.Z;
//!   * timing jitter: cycle-counter deltas (TSC / CNTVCT) across a loop of
//!     memory accesses and branches whose duration depends on cache, TLB,
//!     interrupt and — under a hypervisor — host scheduling state;
//!   * the boot-time clock readings and the tick count.
//! The serial log says once which hardware source (if any) was used.
//!
//! Reseeding. After 1 MiB of output or 60 s, whichever comes first, the key
//! is replaced by BLAKE2s(key || fresh hardware bytes || fresh jitter), so a
//! compromise of the state heals as soon as new entropy arrives.
//!
//! Locking. One global generator under a spinlock, held only while ChaCha20
//! fills a kernel buffer of at most `CHUNK` bytes (a few hundred ns) — never
//! while touching user memory. getrandom(2) is not a hot path (seeds, UUIDs,
//! hash keys, IPC port names), so per-CPU generators would buy nothing but
//! the harder problem of seeding and reseeding N states consistently.
//!
//! Because the generator is seeded before userspace starts, it is always
//! "initialised" in Linux's sense: getrandom never blocks, GRND_NONBLOCK can
//! never see EAGAIN, and GRND_RANDOM draws from the same generator (as
//! /dev/random has on Linux since 5.6).

use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

/// Bytes produced per lock hold.
pub const CHUNK: usize = 256;
const RESEED_BYTES: u64 = 1 << 20;
const RESEED_NS: u64 = 60_000_000_000;

struct Csprng {
    key: [u32; 8],
    since_reseed: u64,
    last_reseed_ns: u64,
}

static RNG: Mutex<Option<Csprng>> = Mutex::new(None);
static LOGGED: AtomicBool = AtomicBool::new(false);

// ── ChaCha20 ────────────────────────────────────────────────────────────────

#[inline(always)]
fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]); s[d] ^= s[a]; s[d] = s[d].rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]); s[b] ^= s[c]; s[b] = s[b].rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]); s[d] ^= s[a]; s[d] = s[d].rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]); s[b] ^= s[c]; s[b] = s[b].rotate_left(7);
}

/// One ChaCha20 block (RFC 8439 layout: 32-bit counter, 96-bit nonce).
pub fn chacha20_block(key: &[u32; 8], counter: u32, nonce: [u32; 3]) -> [u8; 64] {
    let init: [u32; 16] = [
        0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574,
        key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
        counter, nonce[0], nonce[1], nonce[2],
    ];
    let mut s = init;
    for _ in 0..10 {
        qr(&mut s, 0, 4, 8, 12); qr(&mut s, 1, 5, 9, 13);
        qr(&mut s, 2, 6, 10, 14); qr(&mut s, 3, 7, 11, 15);
        qr(&mut s, 0, 5, 10, 15); qr(&mut s, 1, 6, 11, 12);
        qr(&mut s, 2, 7, 8, 13); qr(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

// ── BLAKE2s-256 (RFC 7693), the seed extractor ──────────────────────────────

const B2S_IV: [u32; 8] = [
    0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A,
    0x510E_527F, 0x9B05_688C, 0x1F83_D9AB, 0x5BE0_CD19,
];
const B2S_SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

pub struct Blake2s { h: [u32; 8], t: u64, buf: [u8; 64], n: usize }

impl Blake2s {
    pub fn new() -> Self {
        let mut h = B2S_IV;
        h[0] ^= 0x0101_0000 ^ 32; // digest length 32, no key, fanout/depth 1
        Blake2s { h, t: 0, buf: [0; 64], n: 0 }
    }
    fn compress(&mut self, last: bool) {
        let mut m = [0u32; 16];
        for i in 0..16 { m[i] = u32::from_le_bytes(self.buf[i * 4..i * 4 + 4].try_into().unwrap()); }
        let mut v = [0u32; 16];
        v[..8].copy_from_slice(&self.h);
        v[8..].copy_from_slice(&B2S_IV);
        v[12] ^= self.t as u32;
        v[13] ^= (self.t >> 32) as u32;
        if last { v[14] = !v[14]; }
        let g = |v: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32| {
            v[a] = v[a].wrapping_add(v[b]).wrapping_add(x); v[d] = (v[d] ^ v[a]).rotate_right(16);
            v[c] = v[c].wrapping_add(v[d]); v[b] = (v[b] ^ v[c]).rotate_right(12);
            v[a] = v[a].wrapping_add(v[b]).wrapping_add(y); v[d] = (v[d] ^ v[a]).rotate_right(8);
            v[c] = v[c].wrapping_add(v[d]); v[b] = (v[b] ^ v[c]).rotate_right(7);
        };
        for s in B2S_SIGMA.iter() {
            g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
            g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
            g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
            g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
            g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
            g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
            g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
            g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
        }
        for i in 0..8 { self.h[i] ^= v[i] ^ v[i + 8]; }
    }
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.n == 64 {
                self.t += 64;
                self.compress(false);
                self.n = 0;
            }
            let k = (64 - self.n).min(data.len());
            self.buf[self.n..self.n + k].copy_from_slice(&data[..k]);
            self.n += k;
            data = &data[k..];
        }
    }
    pub fn finalize(mut self) -> [u8; 32] {
        self.t += self.n as u64;
        for b in self.buf[self.n..].iter_mut() { *b = 0; }
        self.compress(true);
        let mut out = [0u8; 32];
        for i in 0..8 { out[i * 4..i * 4 + 4].copy_from_slice(&self.h[i].to_le_bytes()); }
        out
    }
}

// ── Entropy sources ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HwSource { None, Rdseed, Rdrand, Rndrrs, Rndr }

impl HwSource {
    pub fn name(self) -> &'static str {
        match self {
            HwSource::None => "none",
            HwSource::Rdseed => "RDSEED",
            HwSource::Rdrand => "RDRAND",
            HwSource::Rndrrs => "RNDRRS",
            HwSource::Rndr => "RNDR",
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn hw_detect() -> HwSource {
    let ecx1 = core::arch::x86_64::__cpuid(1).ecx;
    let max = core::arch::x86_64::__cpuid(0).eax;
    let ebx7 = if max >= 7 { core::arch::x86_64::__cpuid_count(7, 0).ebx } else { 0 };
    if ebx7 & (1 << 18) != 0 { HwSource::Rdseed }
    else if ecx1 & (1 << 30) != 0 { HwSource::Rdrand }
    else { HwSource::None }
}

#[cfg(target_arch = "x86_64")]
fn hw_u64(src: HwSource) -> Option<u64> {
    // Intel's DRNG guide: RDRAND can fail transiently (retry 10 times);
    // RDSEED fails whenever the conditioner has not refilled (retry with a
    // pause, longer). CF=1 means the value is valid.
    let (tries, seed) = match src {
        HwSource::Rdseed => (128, true),
        HwSource::Rdrand => (10, false),
        _ => return None,
    };
    for _ in 0..tries {
        let v: u64;
        let ok: u8;
        unsafe {
            if seed {
                core::arch::asm!("rdseed {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack));
            } else {
                core::arch::asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack));
            }
        }
        if ok != 0 { return Some(v); }
        core::hint::spin_loop();
    }
    // RDSEED exhausted: RDRAND (if present) is the architected fallback.
    if seed && core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0 {
        return hw_u64(HwSource::Rdrand);
    }
    None
}

#[cfg(target_arch = "x86_64")]
fn cycles() -> u64 { unsafe { core::arch::x86_64::_rdtsc() } }

#[cfg(target_arch = "aarch64")]
fn hw_detect() -> HwSource {
    let isar0: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack)); }
    if (isar0 >> 60) & 0xf >= 1 { HwSource::Rndrrs } else { HwSource::None }
}

#[cfg(target_arch = "aarch64")]
fn hw_u64(src: HwSource) -> Option<u64> {
    // RNDRRS / RNDR (FEAT_RNG): NZCV = 0b0100 (Z set) and a zero result when
    // no random number could be produced in reasonable time. RNDRRS reseeds
    // from the true entropy source first; fall back to RNDR if it keeps
    // failing.
    if src != HwSource::Rndrrs && src != HwSource::Rndr { return None; }
    for attempt in 0..32 {
        let v: u64;
        let fail: u64;
        unsafe {
            if src == HwSource::Rndrrs && attempt < 16 {
                core::arch::asm!("mrs {v}, s3_3_c2_c4_1", "cset {f}, eq", v = out(reg) v, f = out(reg) fail, options(nomem, nostack));
            } else {
                core::arch::asm!("mrs {v}, s3_3_c2_c4_0", "cset {f}, eq", v = out(reg) v, f = out(reg) fail, options(nomem, nostack));
            }
        }
        if fail == 0 { return Some(v); }
        core::hint::spin_loop();
    }
    None
}

#[cfg(target_arch = "aarch64")]
fn cycles() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)); }
    v
}

/// Hash `samples` cycle-counter deltas, each taken across a memory walk whose
/// stride and length depend on the previous delta. Returns the number of
/// distinct low bytes seen, a rough witness that the counter is not
/// perfectly regular.
///
/// The walk spans 64 KiB with a stride that changes every sample, so its
/// duration depends on cache and TLB state as well as interrupts and, under a
/// hypervisor, host scheduling. It is long enough (hundreds of accesses) that
/// even a coarse counter (aarch64's CNTVCT runs at 24 MHz under HVF) sees
/// many ticks per sample, so the low bits of each delta vary.
fn gather_jitter(h: &mut Blake2s, samples: usize) -> usize {
    // Only ever touched with RNG (or, at boot, nothing else) running this
    // code: callers hold the RNG lock.
    static mut SCRATCH: [u8; 65536] = [0; 65536];
    let scratch = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
    let mut seen = [false; 256];
    let mut distinct = 0;
    let mut prev = cycles();
    let mut idx = 0usize;
    for i in 0..samples {
        let steps = 256 + (prev as usize & 255);
        let stride = 64 * (1 + (prev as usize >> 3 & 15)) + 1;
        for _ in 0..steps {
            idx = (idx + stride) & 65535;
            scratch[idx] = scratch[idx].wrapping_add(i as u8) ^ (prev as u8);
            if scratch[idx] & 1 != 0 { idx ^= 0x5a5; }
        }
        let now = cycles();
        let d = now.wrapping_sub(prev);
        h.update(&d.to_le_bytes());
        let lb = (d & 0xff) as usize;
        if !seen[lb] { seen[lb] = true; distinct += 1; }
        prev = now;
    }
    h.update(&scratch[..64]);
    distinct
}

fn gather_hw(h: &mut Blake2s, src: HwSource, words: usize) -> usize {
    let mut got = 0;
    for _ in 0..words {
        if let Some(v) = hw_u64(src) { h.update(&v.to_le_bytes()); got += 1; }
    }
    got
}

fn key_from(digest: [u8; 32]) -> [u32; 8] {
    let mut k = [0u32; 8];
    for i in 0..8 { k[i] = u32::from_le_bytes(digest[i * 4..i * 4 + 4].try_into().unwrap()); }
    k
}

static HW: Mutex<Option<HwSource>> = Mutex::new(None);

fn hw_source() -> HwSource {
    let mut g = HW.lock();
    *g.get_or_insert_with(hw_detect)
}

fn log(s: &str) {
    extern "C" { fn arch_serial_putc(c: u8); }
    for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } }
}
fn log_dec(mut v: usize) {
    let mut buf = [0u8; 20];
    let mut n = 0;
    if v == 0 { buf[0] = b'0'; n = 1; }
    while v > 0 { buf[n] = b'0' + (v % 10) as u8; v /= 10; n += 1; }
    let mut out = [0u8; 20];
    for i in 0..n { out[i] = buf[n - 1 - i]; }
    log(core::str::from_utf8(&out[..n]).unwrap_or("?"));
}

fn seed_key(prev: Option<&[u32; 8]>, hw_words: usize, jitter: usize) -> ([u32; 8], HwSource, usize, usize) {
    let src = hw_source();
    let mut h = Blake2s::new();
    h.update(b"LeandrOS random seed v1");
    if let Some(k) = prev { for w in k { h.update(&w.to_le_bytes()); } }
    let hw = gather_hw(&mut h, src, hw_words);
    let distinct = gather_jitter(&mut h, jitter);
    h.update(&super::monotonic_ns().to_le_bytes());
    h.update(&super::ticks().to_le_bytes());
    h.update(&cycles().to_le_bytes());
    (key_from(h.finalize()), src, hw, distinct)
}

/// Seed the generator. Called once during boot; `fill` seeds lazily too.
pub fn init() {
    let mut g = RNG.lock();
    if g.is_none() { *g = Some(new_state()); }
}

fn new_state() -> Csprng {
    let (key, src, hw, distinct) = seed_key(None, 8, 4096);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log("[RANDOM] ChaCha20 CSPRNG seeded: hardware=");
        log(src.name());
        log(" (");
        log_dec(hw * 8);
        log(" bytes) + jitter (4096 samples, ");
        log_dec(distinct);
        log(" distinct low bytes)");
        if src == HwSource::None || hw == 0 {
            log(" -- NO hardware RNG on this CPU model: timing jitter only");
        }
        log("\n");
    }
    Csprng { key, since_reseed: 0, last_reseed_ns: super::monotonic_ns() }
}

/// Fill `out` (at most CHUNK bytes are produced per lock hold; any length is
/// accepted). Never blocks; safe to call from any process context.
pub fn fill(out: &mut [u8]) {
    for part in out.chunks_mut(CHUNK) { fill_chunk(part); }
}

fn fill_chunk(out: &mut [u8]) {
    let mut g = RNG.lock();
    if g.is_none() { *g = Some(new_state()); }
    let st = g.as_mut().unwrap();
    let now = super::monotonic_ns();
    if st.since_reseed >= RESEED_BYTES || now.wrapping_sub(st.last_reseed_ns) >= RESEED_NS {
        let (k, _, _, _) = seed_key(Some(&st.key), 4, 128);
        st.key = k;
        st.since_reseed = 0;
        st.last_reseed_ns = now;
    }
    // Fast key erasure: block 0's first 32 bytes are the next key; output
    // is the rest of the keystream.
    let mut counter = 0u32;
    let b0 = chacha20_block(&st.key, counter, [0, 0, 0]);
    counter += 1;
    let next_key = key_from(b0[..32].try_into().unwrap());
    let mut pos = 0;
    let first = (out.len()).min(32);
    out[..first].copy_from_slice(&b0[32..32 + first]);
    pos += first;
    while pos < out.len() {
        let b = chacha20_block(&st.key, counter, [0, 0, 0]);
        counter += 1;
        let k = (out.len() - pos).min(64);
        out[pos..pos + k].copy_from_slice(&b[..k]);
        pos += k;
    }
    st.key = next_key;
    st.since_reseed += out.len() as u64;
}
