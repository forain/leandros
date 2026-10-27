//! WPA2-Personal (RSN with CCMP) key management: the supplicant side of the
//! IEEE 802.11 4-way handshake and group key handshake.
//!
//! Everything is hand-rolled (SHA-1, HMAC, PBKDF2, the 802.11 PRF, AES-128,
//! RFC 3394 key wrap) because kernel code may not touch floating point or
//! SIMD registers, which the RustCrypto crates select at runtime on x86_64.
//! Every primitive is checked against its standard test vectors in
//! `drivers/iwlwifi-tests`.
//!
//! Only key descriptor version 2 (HMAC-SHA1-128 MIC, AES key wrap) with CCMP
//! pairwise and group ciphers and a pre-shared key is supported.

use alloc::vec;
use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// SHA-1
// ---------------------------------------------------------------------------

/// Incremental SHA-1 (FIPS 180-4).
#[derive(Clone)]
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

fn sha1_compress(h: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for (i, wi) in w.iter().enumerate() {
        let (f, k) = match i / 20 {
            0 => ((b & c) | (!b & d), 0x5a82_7999u32),
            1 => (b ^ c ^ d, 0x6ed9_eba1),
            2 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let t = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
}

impl Sha1 {
    pub fn new() -> Self {
        Sha1 {
            h: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0],
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut d: &[u8]) {
        self.total = self.total.wrapping_add(d.len() as u64);
        if self.buf_len > 0 {
            let take = core::cmp::min(64 - self.buf_len, d.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&d[..take]);
            self.buf_len += take;
            d = &d[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                sha1_compress(&mut self.h, &block);
                self.buf_len = 0;
            }
        }
        while d.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&d[..64]);
            sha1_compress(&mut self.h, &block);
            d = &d[64..];
        }
        if !d.is_empty() {
            self.buf[..d.len()].copy_from_slice(d);
            self.buf_len = d.len();
        }
    }

    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.total.wrapping_mul(8);
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        let pad_len = if self.buf_len < 56 {
            56 - self.buf_len
        } else {
            120 - self.buf_len
        };
        pad[pad_len..pad_len + 8].copy_from_slice(&bits.to_be_bytes());
        self.update(&pad[..pad_len + 8]);
        let mut out = [0u8; 20];
        for (i, v) in self.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut s = Sha1::new();
    s.update(data);
    s.finish()
}

// ---------------------------------------------------------------------------
// HMAC-SHA1, PBKDF2, PRF
// ---------------------------------------------------------------------------

/// HMAC-SHA1 with the keyed inner/outer states precomputed, so repeated MACs
/// under one key (PBKDF2, PRF) skip the key setup.
#[derive(Clone)]
struct Hmac {
    inner: Sha1,
    outer: Sha1,
}

impl Hmac {
    fn new(key: &[u8]) -> Self {
        let mut k = [0u8; 64];
        if key.len() > 64 {
            k[..20].copy_from_slice(&sha1(key));
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for i in 0..64 {
            ipad[i] ^= k[i];
            opad[i] ^= k[i];
        }
        let mut inner = Sha1::new();
        inner.update(&ipad);
        let mut outer = Sha1::new();
        outer.update(&opad);
        Hmac { inner, outer }
    }

    fn mac(&self, parts: &[&[u8]]) -> [u8; 20] {
        let mut i = self.inner.clone();
        for p in parts {
            i.update(p);
        }
        let mut o = self.outer.clone();
        o.update(&i.finish());
        o.finish()
    }
}

/// HMAC-SHA1 over the concatenation of `parts`.
pub fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    Hmac::new(key).mac(parts)
}

/// PBKDF2 (RFC 2898) with HMAC-SHA1 as the PRF.
pub fn pbkdf2_hmac_sha1(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    let mac = Hmac::new(password);
    for (block, chunk) in out.chunks_mut(20).enumerate() {
        let idx = (block as u32 + 1).to_be_bytes();
        let mut u = mac.mac(&[salt, &idx]);
        let mut t = u;
        for _ in 1..iterations {
            u = mac.mac(&[&u]);
            for j in 0..20 {
                t[j] ^= u[j];
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

/// WPA/WPA2 pre-shared key: PBKDF2(passphrase, ssid, 4096, 32).
pub fn wpa_psk(passphrase: &[u8], ssid: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2_hmac_sha1(passphrase, ssid, 4096, &mut out);
    out
}

/// IEEE 802.11 PRF-n (SHA-1 based, 802.11-2016 12.7.1.2): `out.len()` bytes of
/// HMAC-SHA1(key, label || 0x00 || data || i) for i = 0, 1, ...
pub fn prf_sha1(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    let mac = Hmac::new(key);
    for (i, chunk) in out.chunks_mut(20).enumerate() {
        let counter = [i as u8];
        let t = mac.mac(&[label, &[0u8], data, &counter]);
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

/// Pairwise transient key split for CCMP.
pub struct Ptk {
    pub kck: [u8; 16],
    pub kek: [u8; 16],
    pub tk: [u8; 16],
}

/// PTK for CCMP (384 bits) = PRF-384(PMK, "Pairwise key expansion",
/// min(AA,SPA) || max(AA,SPA) || min(ANonce,SNonce) || max(ANonce,SNonce)).
pub fn derive_ptk(
    pmk: &[u8; 32],
    aa: &[u8; 6],
    spa: &[u8; 6],
    anonce: &[u8; 32],
    snonce: &[u8; 32],
) -> Ptk {
    let data = ptk_context(aa, spa, anonce, snonce);
    let mut out = [0u8; 48];
    prf_sha1(pmk, b"Pairwise key expansion", &data, &mut out);
    split_ptk(&out)
}

// ---------------------------------------------------------------------------
// SHA-256, HMAC-SHA256, the IEEE 802.11 KDF (AKM 6, PSK-SHA256), AES-CMAC
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Incremental SHA-256 (FIPS 180-4).
#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

fn sha256_compress(h: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA256_K[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (x, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *x = x.wrapping_add(v);
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut d: &[u8]) {
        self.total = self.total.wrapping_add(d.len() as u64);
        while !d.is_empty() {
            let take = (64 - self.buf_len).min(d.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&d[..take]);
            self.buf_len += take;
            d = &d[take..];
            if self.buf_len == 64 {
                let b = self.buf;
                sha256_compress(&mut self.h, &b);
                self.buf_len = 0;
            }
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (i, w) in self.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut s = Sha256::new();
    s.update(data);
    s.finish()
}

/// HMAC-SHA256 (RFC 2104) over the concatenation of `parts`.
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; 64];
    let mut opad = [0u8; 64];
    for i in 0..64 {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5c;
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    for p in parts {
        inner.update(p);
    }
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner.finish());
    outer.finish()
}

/// IEEE 802.11 KDF-SHA256-Length (802.11-2016 12.7.1.6.2): `out.len()` bytes
/// of HMAC-SHA256(key, i || label || context || Length) for i = 1, 2, ...,
/// with i and Length (in bits) as 16-bit little-endian values.
pub fn kdf_sha256(key: &[u8], label: &[u8], context: &[u8], out: &mut [u8]) {
    let bits = ((out.len() * 8) as u16).to_le_bytes();
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let ctr = ((i + 1) as u16).to_le_bytes();
        let t = hmac_sha256(key, &[&ctr, label, context, &bits]);
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

/// PTK for AKM 6 (PSK-SHA256) with CCMP = KDF-SHA256-384(PMK, "Pairwise key
/// expansion", min(AA,SPA) || max(AA,SPA) || min(ANonce,SNonce) || max(ANonce,SNonce)).
pub fn derive_ptk_sha256(
    pmk: &[u8; 32],
    aa: &[u8; 6],
    spa: &[u8; 6],
    anonce: &[u8; 32],
    snonce: &[u8; 32],
) -> Ptk {
    let data = ptk_context(aa, spa, anonce, snonce);
    let mut out = [0u8; 48];
    kdf_sha256(pmk, b"Pairwise key expansion", &data, &mut out);
    split_ptk(&out)
}

fn ptk_context(aa: &[u8; 6], spa: &[u8; 6], anonce: &[u8; 32], snonce: &[u8; 32]) -> [u8; 76] {
    let mut data = [0u8; 76];
    let (a_lo, a_hi) = if aa <= spa { (aa, spa) } else { (spa, aa) };
    let (n_lo, n_hi) = if anonce <= snonce { (anonce, snonce) } else { (snonce, anonce) };
    data[0..6].copy_from_slice(a_lo);
    data[6..12].copy_from_slice(a_hi);
    data[12..44].copy_from_slice(n_lo);
    data[44..76].copy_from_slice(n_hi);
    data
}

fn split_ptk(out: &[u8; 48]) -> Ptk {
    let mut ptk = Ptk { kck: [0; 16], kek: [0; 16], tk: [0; 16] };
    ptk.kck.copy_from_slice(&out[0..16]);
    ptk.kek.copy_from_slice(&out[16..32]);
    ptk.tk.copy_from_slice(&out[32..48]);
    ptk
}

/// AES-128-CMAC (RFC 4493 / NIST SP 800-38B) over the concatenation of `parts`.
pub fn aes_cmac(key: &[u8; 16], parts: &[&[u8]]) -> [u8; 16] {
    let aes = Aes128::new(key);
    let dbl = |b: [u8; 16]| -> [u8; 16] {
        let mut o = [0u8; 16];
        for i in 0..16 {
            o[i] = (b[i] << 1) | if i < 15 { b[i + 1] >> 7 } else { 0 };
        }
        if b[0] & 0x80 != 0 {
            o[15] ^= 0x87;
        }
        o
    };
    let mut l = [0u8; 16];
    aes.encrypt_block(&mut l);
    let k1 = dbl(l);
    let k2 = dbl(k1);
    let mut x = [0u8; 16];
    let mut blk = [0u8; 16];
    let mut fill = 0usize;
    for b in parts.iter().flat_map(|p| p.iter()) {
        if fill == 16 {
            for i in 0..16 {
                x[i] ^= blk[i];
            }
            aes.encrypt_block(&mut x);
            fill = 0;
        }
        blk[fill] = *b;
        fill += 1;
    }
    // Last block: complete -> XOR K1; partial or empty -> pad 10* and XOR K2.
    let k = if fill == 16 {
        k1
    } else {
        blk[fill] = 0x80;
        for v in blk.iter_mut().skip(fill + 1) {
            *v = 0;
        }
        k2
    };
    for i in 0..16 {
        x[i] ^= blk[i] ^ k[i];
    }
    aes.encrypt_block(&mut x);
    x
}

// ---------------------------------------------------------------------------
// AES-128
// ---------------------------------------------------------------------------

const fn xtime(a: u8) -> u8 {
    (a << 1) ^ if a & 0x80 != 0 { 0x1b } else { 0 }
}

const fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    r
}

const fn build_sbox() -> [u8; 256] {
    let mut s = [0u8; 256];
    let mut x = 0usize;
    while x < 256 {
        // Multiplicative inverse as x^254 (0 maps to 0).
        let a = x as u8;
        let mut inv = 1u8;
        let mut i = 0;
        while i < 254 {
            inv = gmul(inv, a);
            i += 1;
        }
        if x == 0 {
            inv = 0;
        }
        s[x] = inv
            ^ inv.rotate_left(1)
            ^ inv.rotate_left(2)
            ^ inv.rotate_left(3)
            ^ inv.rotate_left(4)
            ^ 0x63;
        x += 1;
    }
    s
}

const fn build_inv_sbox() -> [u8; 256] {
    let s = build_sbox();
    let mut inv = [0u8; 256];
    let mut x = 0usize;
    while x < 256 {
        inv[s[x] as usize] = x as u8;
        x += 1;
    }
    inv
}

static SBOX: [u8; 256] = build_sbox();
static INV_SBOX: [u8; 256] = build_inv_sbox();

/// AES-128 with the key schedule expanded once.
pub struct Aes128 {
    round_keys: [[u8; 16]; 11],
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Self {
        let mut rk = [[0u8; 16]; 11];
        rk[0] = *key;
        let mut rcon = 1u8;
        for r in 1..11 {
            let prev = rk[r - 1];
            let mut t = [
                SBOX[prev[13] as usize] ^ rcon,
                SBOX[prev[14] as usize],
                SBOX[prev[15] as usize],
                SBOX[prev[12] as usize],
            ];
            rcon = xtime(rcon);
            let mut cur = [0u8; 16];
            for c in 0..4 {
                for j in 0..4 {
                    t[j] ^= prev[4 * c + j];
                    cur[4 * c + j] = t[j];
                }
            }
            rk[r] = cur;
        }
        Aes128 { round_keys: rk }
    }

    fn add_round_key(b: &mut [u8; 16], k: &[u8; 16]) {
        for i in 0..16 {
            b[i] ^= k[i];
        }
    }

    pub fn encrypt_block(&self, b: &mut [u8; 16]) {
        Self::add_round_key(b, &self.round_keys[0]);
        for round in 1..11 {
            for x in b.iter_mut() {
                *x = SBOX[*x as usize];
            }
            let o = *b;
            for c in 0..4 {
                for r in 0..4 {
                    b[4 * c + r] = o[4 * ((c + r) % 4) + r];
                }
            }
            if round != 10 {
                for c in 0..4 {
                    let a = [b[4 * c], b[4 * c + 1], b[4 * c + 2], b[4 * c + 3]];
                    b[4 * c] = xtime(a[0]) ^ (xtime(a[1]) ^ a[1]) ^ a[2] ^ a[3];
                    b[4 * c + 1] = a[0] ^ xtime(a[1]) ^ (xtime(a[2]) ^ a[2]) ^ a[3];
                    b[4 * c + 2] = a[0] ^ a[1] ^ xtime(a[2]) ^ (xtime(a[3]) ^ a[3]);
                    b[4 * c + 3] = (xtime(a[0]) ^ a[0]) ^ a[1] ^ a[2] ^ xtime(a[3]);
                }
            }
            Self::add_round_key(b, &self.round_keys[round]);
        }
    }

    pub fn decrypt_block(&self, b: &mut [u8; 16]) {
        Self::add_round_key(b, &self.round_keys[10]);
        for round in (0..10).rev() {
            let o = *b;
            for c in 0..4 {
                for r in 0..4 {
                    b[4 * ((c + r) % 4) + r] = o[4 * c + r];
                }
            }
            for x in b.iter_mut() {
                *x = INV_SBOX[*x as usize];
            }
            Self::add_round_key(b, &self.round_keys[round]);
            if round != 0 {
                for c in 0..4 {
                    let a = [b[4 * c], b[4 * c + 1], b[4 * c + 2], b[4 * c + 3]];
                    for r in 0..4 {
                        b[4 * c + r] = gmul(a[r], 14)
                            ^ gmul(a[(r + 1) % 4], 11)
                            ^ gmul(a[(r + 2) % 4], 13)
                            ^ gmul(a[(r + 3) % 4], 9);
                    }
                }
            }
        }
    }
}

const KEY_WRAP_IV: [u8; 8] = [0xa6; 8];

/// RFC 3394 AES key wrap. `plain` must be a multiple of 8 bytes, >= 16.
pub fn aes_key_wrap(kek: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let aes = Aes128::new(kek);
    let n = plain.len() / 8;
    let mut a = KEY_WRAP_IV;
    let mut r: Vec<[u8; 8]> = (0..n)
        .map(|i| {
            let mut x = [0u8; 8];
            x.copy_from_slice(&plain[8 * i..8 * i + 8]);
            x
        })
        .collect();
    for j in 0..6u64 {
        for i in 0..n {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&a);
            b[8..].copy_from_slice(&r[i]);
            aes.encrypt_block(&mut b);
            let t = (n as u64) * j + i as u64 + 1;
            a.copy_from_slice(&b[..8]);
            for (x, y) in a.iter_mut().zip(t.to_be_bytes()) {
                *x ^= y;
            }
            r[i].copy_from_slice(&b[8..]);
        }
    }
    let mut out = Vec::with_capacity(8 * (n + 1));
    out.extend_from_slice(&a);
    for blk in &r {
        out.extend_from_slice(blk);
    }
    out
}

/// RFC 3394 AES key unwrap. `None` on bad length or integrity failure.
pub fn aes_key_unwrap(kek: &[u8; 16], wrapped: &[u8]) -> Option<Vec<u8>> {
    if wrapped.len() % 8 != 0 || wrapped.len() < 24 {
        return None;
    }
    let aes = Aes128::new(kek);
    let n = wrapped.len() / 8 - 1;
    let mut a = [0u8; 8];
    a.copy_from_slice(&wrapped[..8]);
    let mut r: Vec<[u8; 8]> = (0..n)
        .map(|i| {
            let mut x = [0u8; 8];
            x.copy_from_slice(&wrapped[8 * (i + 1)..8 * (i + 2)]);
            x
        })
        .collect();
    for j in (0..6u64).rev() {
        for i in (0..n).rev() {
            let t = ((n as u64) * j + i as u64 + 1).to_be_bytes();
            let mut b = [0u8; 16];
            for k in 0..8 {
                b[k] = a[k] ^ t[k];
            }
            b[8..].copy_from_slice(&r[i]);
            aes.decrypt_block(&mut b);
            a.copy_from_slice(&b[..8]);
            r[i].copy_from_slice(&b[8..]);
        }
    }
    let mut diff = 0u8;
    for k in 0..8 {
        diff |= a[k] ^ KEY_WRAP_IV[k];
    }
    if diff != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(8 * n);
    for blk in &r {
        out.extend_from_slice(blk);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// EAPOL-Key frames
// ---------------------------------------------------------------------------

pub const ETHERTYPE_EAPOL: u16 = 0x888e;

/// Key Information field bits (descriptor version in bits 0..2).
pub mod key_info {
    pub const VERSION_MASK: u16 = 0x0007;
    pub const TYPE_PAIRWISE: u16 = 1 << 3;
    pub const INSTALL: u16 = 1 << 6;
    pub const ACK: u16 = 1 << 7;
    pub const MIC: u16 = 1 << 8;
    pub const SECURE: u16 = 1 << 9;
    pub const ERROR: u16 = 1 << 10;
    pub const REQUEST: u16 = 1 << 11;
    pub const ENCRYPTED_KEY_DATA: u16 = 1 << 12;
}

/// EAPOL header (4) + fixed key descriptor fields up to and including the
/// key data length (95).
const EAPOL_KEY_FIXED: usize = 99;
const MIC_OFFSET: usize = 81;
const EAPOL_TYPE_KEY: u8 = 3;
const DESC_RSN: u8 = 2;
const DESC_WPA: u8 = 254;

/// A parsed EAPOL-Key frame; `key_data` borrows from the input PDU.
pub struct EapolKey<'a> {
    pub version: u8,
    pub descriptor_type: u8,
    pub key_info: u16,
    pub key_len: u16,
    pub replay_counter: [u8; 8],
    pub nonce: [u8; 32],
    pub iv: [u8; 16],
    pub rsc: [u8; 8],
    pub mic: [u8; 16],
    pub key_data: &'a [u8],
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// Length of the EAPOL PDU proper (header + body), excluding any link-layer
/// padding after it. `None` if the header is inconsistent.
fn eapol_pdu_len(pdu: &[u8]) -> Option<usize> {
    if pdu.len() < EAPOL_KEY_FIXED || pdu[1] != EAPOL_TYPE_KEY {
        return None;
    }
    let total = 4 + be16(&pdu[2..4]) as usize;
    if total < EAPOL_KEY_FIXED || total > pdu.len() {
        return None;
    }
    Some(total)
}

/// `pdu` starts at the EAPOL protocol-version byte (after LLC/SNAP and the
/// ethertype). Accepts descriptor type 2 (RSN) and 254 (WPA); callers may
/// reject the latter.
pub fn parse_eapol_key(pdu: &[u8]) -> Option<EapolKey<'_>> {
    let total = eapol_pdu_len(pdu)?;
    let pdu = &pdu[..total];
    let descriptor_type = pdu[4];
    if descriptor_type != DESC_RSN && descriptor_type != DESC_WPA {
        return None;
    }
    let data_len = be16(&pdu[97..99]) as usize;
    if EAPOL_KEY_FIXED + data_len > total {
        return None;
    }
    let mut replay_counter = [0u8; 8];
    replay_counter.copy_from_slice(&pdu[9..17]);
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&pdu[17..49]);
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&pdu[49..65]);
    let mut rsc = [0u8; 8];
    rsc.copy_from_slice(&pdu[65..73]);
    let mut mic = [0u8; 16];
    mic.copy_from_slice(&pdu[MIC_OFFSET..MIC_OFFSET + 16]);
    Some(EapolKey {
        version: pdu[0],
        descriptor_type,
        key_info: be16(&pdu[5..7]),
        key_len: be16(&pdu[7..9]),
        replay_counter,
        nonce,
        iv,
        rsc,
        mic,
        key_data: &pdu[EAPOL_KEY_FIXED..EAPOL_KEY_FIXED + data_len],
    })
}

/// EAPOL-Key MIC over the PDU with the MIC field zeroed, by the key
/// descriptor version in its Key Information: 2 = HMAC-SHA1-128,
/// 3 = AES-128-CMAC (AKM 6 / PMF SHA-256 AKMs).
fn compute_mic(kck: &[u8; 16], pdu: &[u8]) -> Option<[u8; 16]> {
    let total = eapol_pdu_len(pdu)?;
    let pdu = &pdu[..total];
    let ver = be16(&pdu[5..7]) & key_info::VERSION_MASK;
    let parts: [&[u8]; 3] = [&pdu[..MIC_OFFSET], &[0u8; 16], &pdu[MIC_OFFSET + 16..]];
    let mut mic = [0u8; 16];
    match ver {
        2 => mic.copy_from_slice(&hmac_sha1(kck, &parts)[..16]),
        3 => mic = aes_cmac(kck, &parts),
        _ => return None,
    }
    Some(mic)
}

pub fn eapol_mic_ok(kck: &[u8; 16], pdu: &[u8]) -> bool {
    let Some(want) = compute_mic(kck, pdu) else {
        return false;
    };
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= want[i] ^ pdu[MIC_OFFSET + i];
    }
    diff == 0
}

pub fn eapol_set_mic(kck: &[u8; 16], pdu: &mut [u8]) {
    if let Some(mic) = compute_mic(kck, pdu) {
        pdu[MIC_OFFSET..MIC_OFFSET + 16].copy_from_slice(&mic);
    }
}

/// Build an EAPOL-Key PDU (descriptor type 2, MIC field zero; call
/// `eapol_set_mic` afterwards when `key_info::MIC` is set). `eapol_version`
/// is the protocol version byte, normally the one the AP used.
pub fn build_eapol_key(
    eapol_version: u8,
    key_info: u16,
    key_len: u16,
    replay_counter: &[u8; 8],
    nonce: &[u8; 32],
    key_data: &[u8],
) -> Vec<u8> {
    let body_len = (EAPOL_KEY_FIXED - 4 + key_data.len()) as u16;
    let mut v = Vec::with_capacity(EAPOL_KEY_FIXED + key_data.len());
    v.push(eapol_version);
    v.push(EAPOL_TYPE_KEY);
    v.extend_from_slice(&body_len.to_be_bytes());
    v.push(DESC_RSN);
    v.extend_from_slice(&key_info.to_be_bytes());
    v.extend_from_slice(&key_len.to_be_bytes());
    v.extend_from_slice(replay_counter);
    v.extend_from_slice(nonce);
    v.extend_from_slice(&[0u8; 16]); // key IV
    v.extend_from_slice(&[0u8; 8]); // key RSC
    v.extend_from_slice(&[0u8; 8]); // reserved
    v.extend_from_slice(&[0u8; 16]); // MIC
    v.extend_from_slice(&(key_data.len() as u16).to_be_bytes());
    v.extend_from_slice(key_data);
    v
}

/// RSN IE for WPA2-PSK with CCMP pairwise and group ciphers.
pub fn rsn_ie_psk_ccmp() -> [u8; 22] {
    rsn_ie_psk(0x000f_ac04)
}

/// RSN IE for WPA2-PSK with CCMP pairwise and the given group cipher suite
/// (CCMP, or TKIP for a WPA/WPA2 mixed-mode network: the station must echo
/// the AP's group cipher).
pub fn rsn_ie_psk(group_suite: u32) -> [u8; 22] {
    let mut ie = [
        0x30, 0x14, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01,
        0x00, 0x00, 0x0f, 0xac, 0x02, 0x00, 0x00,
    ];
    ie[4..8].copy_from_slice(&group_suite.to_be_bytes());
    ie
}

/// RSN IE for a PSK network: CCMP pairwise, `group_suite` group cipher, one
/// AKM (`akm` = suite type: 2 PSK, 6 PSK-SHA256) and the RSN capabilities
/// (`caps`, little-endian on the wire; bit 7 MFPC, bit 6 MFPR). Without a
/// PMKID list or group management cipher (BIP-CMAC-128 is the default).
pub fn rsn_ie_psk_akm(group_suite: u32, akm: u8, caps: u16) -> [u8; 22] {
    let mut ie = rsn_ie_psk(group_suite);
    ie[19] = akm;
    ie[20..22].copy_from_slice(&caps.to_le_bytes());
    ie
}

/// RSN capability bits.
pub const RSN_CAP_MFPR: u16 = 1 << 6;
pub const RSN_CAP_MFPC: u16 = 1 << 7;

/// An integrity group temporal key (PMF/BIP) from an IGTK KDE.
pub struct Igtk {
    /// 4 or 5.
    pub key_id: u16,
    /// IGTK packet number, little-endian (IPN[0] is the low byte).
    pub ipn: [u8; 6],
    pub key: [u8; 16],
}

/// Find the IGTK KDE (`dd len 00-0f-ac 09 {keyid(2, LE), ipn(6), igtk(16)}`)
/// in decrypted key data.
pub fn find_igtk_kde(key_data: &[u8]) -> Option<Igtk> {
    let mut p = 0usize;
    while p + 2 <= key_data.len() {
        let tag = key_data[p];
        let len = key_data[p + 1] as usize;
        if tag == 0xdd && len == 0 {
            return None;
        }
        let end = p + 2 + len;
        if end > key_data.len() {
            return None;
        }
        let body = &key_data[p + 2..end];
        if tag == 0xdd && body.len() >= 4 + 24 && body[..4] == [0x00, 0x0f, 0xac, 0x09] {
            let mut ipn = [0u8; 6];
            ipn.copy_from_slice(&body[6..12]);
            let mut key = [0u8; 16];
            key.copy_from_slice(&body[12..28]);
            return Some(Igtk { key_id: u16::from_le_bytes([body[4], body[5]]), ipn, key });
        }
        p = end;
    }
    None
}

/// A group temporal key from a GTK KDE.
pub struct Gtk {
    pub key_id: u8,
    pub tx: bool,
    pub key: Vec<u8>,
}

/// Find the GTK KDE (`dd len 00-0f-ac 01 {keyid | tx<<2, 0, gtk}`) in
/// decrypted key data. An `0xdd` element of length 0 is padding and ends the
/// scan.
pub fn find_gtk_kde(key_data: &[u8]) -> Option<Gtk> {
    let mut p = 0usize;
    while p + 2 <= key_data.len() {
        let tag = key_data[p];
        let len = key_data[p + 1] as usize;
        if tag == 0xdd && len == 0 {
            return None;
        }
        let end = p + 2 + len;
        if end > key_data.len() {
            return None;
        }
        let body = &key_data[p + 2..end];
        if tag == 0xdd && body.len() > 6 && body[..4] == [0x00, 0x0f, 0xac, 0x01] {
            return Some(Gtk {
                key_id: body[4] & 0x03,
                tx: body[4] & 0x04 != 0,
                key: body[6..].to_vec(),
            });
        }
        p = end;
    }
    None
}

// ---------------------------------------------------------------------------
// Supplicant state machine
// ---------------------------------------------------------------------------

/// Action requested of the caller by the supplicant.
pub enum HsEvent {
    /// Transmit this EAPOL PDU (caller wraps it in LLC/SNAP + 802.11 data).
    Send(Vec<u8>),
    /// Install the pairwise temporal key (16-byte CCMP TK). Emitted after
    /// message 3 is verified, after the `Send(msg4)` event: the caller
    /// transmits message 4 unencrypted first, then installs the key.
    InstallPtk([u8; 16]),
    /// Install a group key (16-byte CCMP GTK, or 32-byte TKIP GTK in the
    /// KDE layout, see `TkipKey::from_gtk`) with its key index and receive
    /// sequence counter.
    InstallGtk {
        key_id: u8,
        key: Vec<u8>,
        rsc: [u8; 8],
    },
    /// Install an integrity group key (PMF, BIP-CMAC-128): key id 4 or 5,
    /// the 16-byte IGTK and its 6-byte packet number (little-endian).
    InstallIgtk {
        key_id: u16,
        key: [u8; 16],
        ipn: [u8; 6],
    },
    /// The 4-way handshake completed (port authorized).
    Complete,
    /// A protocol error the caller should log.
    Error(&'static str),
}

pub struct Supplicant {
    pmk: [u8; 32],
    own_addr: [u8; 6],
    ap_addr: [u8; 6],
    own_rsn_ie: Vec<u8>,
    snonce: [u8; 32],
    anonce: Option<[u8; 32]>,
    ptk: Option<Ptk>,
    /// Last accepted replay counter.
    replay: Option<u64>,
    complete: bool,
    /// Expected GTK length: 16 (CCMP group) or 32 (TKIP group).
    gtk_len: usize,
    /// Negotiated AKM suite type (2 PSK, 6 PSK-SHA256); 0 = unknown, the
    /// descriptor version of message 1 decides.
    akm: u8,
    /// Key descriptor version in use (from message 1): 2 or 3.
    ver: u16,
}

const GTK_LEN: usize = 16;

impl Supplicant {
    /// `own_rsn_ie` is exactly the RSN IE sent in the association request.
    /// `snonce` must be 32 random bytes.
    pub fn new(
        pmk: [u8; 32],
        own_addr: [u8; 6],
        ap_addr: [u8; 6],
        own_rsn_ie: Vec<u8>,
        snonce: [u8; 32],
    ) -> Self {
        Supplicant {
            pmk,
            own_addr,
            ap_addr,
            own_rsn_ie,
            snonce,
            anonce: None,
            ptk: None,
            replay: None,
            complete: false,
            gtk_len: GTK_LEN,
            akm: 0,
            ver: 0,
        }
    }

    /// The network's group cipher is TKIP (WPA/WPA2 mixed mode): expect a
    /// 32-byte TKIP GTK. The EAPOL-Key descriptor stays version 2
    /// (HMAC-SHA1 MIC, AES key wrap) because the pairwise cipher is CCMP.
    pub fn set_group_tkip(&mut self, tkip: bool) {
        self.gtk_len = if tkip { TKIP_GTK_LEN } else { GTK_LEN };
    }

    /// The negotiated AKM (from the association request's RSN IE): 6
    /// (PSK-SHA256) derives the PTK with KDF-SHA256 even if message 1 were
    /// to carry descriptor version 2.
    pub fn set_akm(&mut self, akm: u8) {
        self.akm = akm;
    }

    /// Key descriptor version seen in message 1 (0 before it).
    pub fn descriptor_version(&self) -> u16 {
        self.ver
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    fn err(msg: &'static str) -> Vec<HsEvent> {
        vec![HsEvent::Error(msg)]
    }

    fn replay_fresh(&self, rc: &[u8; 8]) -> bool {
        match self.replay {
            None => true,
            Some(last) => u64::from_be_bytes(*rc) > last,
        }
    }

    /// Feed one received EAPOL PDU from the AP; returns events in order.
    pub fn on_eapol(&mut self, pdu: &[u8]) -> Vec<HsEvent> {
        let Some(k) = parse_eapol_key(pdu) else {
            return Self::err("malformed EAPOL-Key");
        };
        if k.descriptor_type != DESC_RSN {
            return Self::err("unsupported key descriptor (WPA1)");
        }
        let ver = k.key_info & key_info::VERSION_MASK;
        if ver != 2 && ver != 3 {
            return Self::err("unsupported key descriptor version");
        }
        if self.ver != 0 && ver != self.ver {
            return Self::err("key descriptor version changed mid-handshake");
        }
        let ki = k.key_info;
        let has = |f: u16| ki & f == f;
        if has(key_info::TYPE_PAIRWISE) {
            if has(key_info::ACK) && !has(key_info::MIC) {
                self.on_msg1(&k)
            } else if has(key_info::ACK | key_info::MIC | key_info::INSTALL) {
                self.on_msg3(&k, pdu)
            } else {
                Self::err("unexpected pairwise EAPOL-Key")
            }
        } else if has(key_info::ACK | key_info::MIC | key_info::SECURE) {
            self.on_group(&k, pdu)
        } else {
            Self::err("unexpected group EAPOL-Key")
        }
    }

    fn on_msg1(&mut self, k: &EapolKey<'_>) -> Vec<HsEvent> {
        if self.complete {
            return Self::err("pairwise rekey not supported");
        }
        let ver = k.key_info & key_info::VERSION_MASK;
        let ptk = if self.akm == 6 || ver == 3 {
            derive_ptk_sha256(&self.pmk, &self.ap_addr, &self.own_addr, &k.nonce, &self.snonce)
        } else {
            derive_ptk(&self.pmk, &self.ap_addr, &self.own_addr, &k.nonce, &self.snonce)
        };
        self.ver = ver;
        self.anonce = Some(k.nonce);
        self.replay = Some(u64::from_be_bytes(k.replay_counter));
        let mut msg2 = build_eapol_key(
            k.version,
            self.ver | key_info::TYPE_PAIRWISE | key_info::MIC,
            0,
            &k.replay_counter,
            &self.snonce,
            &self.own_rsn_ie,
        );
        eapol_set_mic(&ptk.kck, &mut msg2);
        self.ptk = Some(ptk);
        vec![HsEvent::Send(msg2)]
    }

    fn on_msg3(&mut self, k: &EapolKey<'_>, pdu: &[u8]) -> Vec<HsEvent> {
        let (Some(ptk), Some(anonce)) = (self.ptk.as_ref(), self.anonce) else {
            return Self::err("message 3 before message 1");
        };
        if k.nonce != anonce {
            return Self::err("message 3 ANonce mismatch");
        }
        if !eapol_mic_ok(&ptk.kck, pdu) {
            return Self::err("message 3 MIC failure");
        }
        if !self.replay_fresh(&k.replay_counter) {
            return Self::err("message 3 replayed");
        }
        let data = if k.key_info & key_info::ENCRYPTED_KEY_DATA != 0 {
            match aes_key_unwrap(&ptk.kek, k.key_data) {
                Some(d) => d,
                None => return Self::err("message 3 key data unwrap failed"),
            }
        } else {
            k.key_data.to_vec()
        };
        let Some(gtk) = find_gtk_kde(&data) else {
            return Self::err("message 3 has no GTK");
        };
        if gtk.key.len() != self.gtk_len {
            return Self::err("unsupported GTK length");
        }
        self.replay = Some(u64::from_be_bytes(k.replay_counter));

        let mut msg4 = build_eapol_key(
            k.version,
            self.ver | key_info::TYPE_PAIRWISE | key_info::MIC | key_info::SECURE,
            0,
            &k.replay_counter,
            &[0u8; 32],
            &[],
        );
        eapol_set_mic(&ptk.kck, &mut msg4);
        let mut ev = vec![HsEvent::Send(msg4)];
        if !self.complete {
            self.complete = true;
            ev.push(HsEvent::InstallPtk(ptk.tk));
            ev.push(HsEvent::InstallGtk {
                key_id: gtk.key_id,
                key: gtk.key,
                rsc: k.rsc,
            });
            if let Some(ig) = find_igtk_kde(&data) {
                ev.push(HsEvent::InstallIgtk { key_id: ig.key_id, key: ig.key, ipn: ig.ipn });
            }
            ev.push(HsEvent::Complete);
        }
        ev
    }

    fn on_group(&mut self, k: &EapolKey<'_>, pdu: &[u8]) -> Vec<HsEvent> {
        let (true, Some(ptk)) = (self.complete, self.ptk.as_ref()) else {
            return Self::err("group key message before handshake completion");
        };
        if !eapol_mic_ok(&ptk.kck, pdu) {
            return Self::err("group message MIC failure");
        }
        if !self.replay_fresh(&k.replay_counter) {
            return Self::err("group message replayed");
        }
        let data = if k.key_info & key_info::ENCRYPTED_KEY_DATA != 0 {
            match aes_key_unwrap(&ptk.kek, k.key_data) {
                Some(d) => d,
                None => return Self::err("group key data unwrap failed"),
            }
        } else {
            k.key_data.to_vec()
        };
        let Some(gtk) = find_gtk_kde(&data) else {
            return Self::err("group message has no GTK");
        };
        if gtk.key.len() != self.gtk_len {
            return Self::err("unsupported GTK length");
        }
        self.replay = Some(u64::from_be_bytes(k.replay_counter));
        let mut reply = build_eapol_key(
            k.version,
            self.ver | key_info::MIC | key_info::SECURE,
            0,
            &k.replay_counter,
            &[0u8; 32],
            &[],
        );
        eapol_set_mic(&ptk.kck, &mut reply);
        let mut ev = vec![
            HsEvent::Send(reply),
            HsEvent::InstallGtk {
                key_id: gtk.key_id,
                key: gtk.key,
                rsc: k.rsc,
            },
        ];
        if let Some(ig) = find_igtk_kde(&data) {
            ev.push(HsEvent::InstallIgtk { key_id: ig.key_id, key: ig.key, ipn: ig.ipn });
        }
        ev
    }
}

// ---------------------------------------------------------------------------
// TKIP (IEEE 802.11-2016 12.5.2), receive side only: a WPA/WPA2 mixed-mode
// network (pairwise CCMP, group TKIP) sends broadcast/multicast with a TKIP
// GTK, and a station never transmits with the GTK.
// ---------------------------------------------------------------------------

/// Length of a TKIP GTK in the GTK KDE: TK (16) | authenticator TX MIC key
/// (8) | authenticator RX MIC key (8).
pub const TKIP_GTK_LEN: usize = 32;

/// A TKIP group key from the station's point of view. The station receives
/// what the authenticator transmits, so its RX Michael key is the
/// authenticator's TX key (GTK bytes 16..24), as wpa_supplicant swaps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TkipKey {
    pub tk: [u8; 16],
    pub rx_mic: [u8; 8],
    pub tx_mic: [u8; 8],
}

impl TkipKey {
    pub fn from_gtk(gtk: &[u8]) -> Option<TkipKey> {
        if gtk.len() != TKIP_GTK_LEN {
            return None;
        }
        let mut k = TkipKey { tk: [0; 16], rx_mic: [0; 8], tx_mic: [0; 8] };
        k.tk.copy_from_slice(&gtk[..16]);
        k.rx_mic.copy_from_slice(&gtk[16..24]);
        k.tx_mic.copy_from_slice(&gtk[24..32]);
        Some(k)
    }

    /// The 32-byte key as the driver/firmware expects it (the nl80211
    /// layout iwlmvm copies into ADD_STA_KEY): TK, the station's TX MIC
    /// key at 16, its RX MIC key at 24.
    pub fn driver_layout(&self) -> [u8; 32] {
        let mut k = [0u8; 32];
        k[..16].copy_from_slice(&self.tk);
        k[16..24].copy_from_slice(&self.tx_mic);
        k[24..32].copy_from_slice(&self.rx_mic);
        k
    }
}

const TKIP_SBOX: [u16; 256] = [
    0xC6A5, 0xF884, 0xEE99, 0xF68D, 0xFF0D, 0xD6BD, 0xDEB1, 0x9154,
    0x6050, 0x0203, 0xCEA9, 0x567D, 0xE719, 0xB562, 0x4DE6, 0xEC9A,
    0x8F45, 0x1F9D, 0x8940, 0xFA87, 0xEF15, 0xB2EB, 0x8EC9, 0xFB0B,
    0x41EC, 0xB367, 0x5FFD, 0x45EA, 0x23BF, 0x53F7, 0xE496, 0x9B5B,
    0x75C2, 0xE11C, 0x3DAE, 0x4C6A, 0x6C5A, 0x7E41, 0xF502, 0x834F,
    0x685C, 0x51F4, 0xD134, 0xF908, 0xE293, 0xAB73, 0x6253, 0x2A3F,
    0x080C, 0x9552, 0x4665, 0x9D5E, 0x3028, 0x37A1, 0x0A0F, 0x2FB5,
    0x0E09, 0x2436, 0x1B9B, 0xDF3D, 0xCD26, 0x4E69, 0x7FCD, 0xEA9F,
    0x121B, 0x1D9E, 0x5874, 0x342E, 0x362D, 0xDCB2, 0xB4EE, 0x5BFB,
    0xA4F6, 0x764D, 0xB761, 0x7DCE, 0x527B, 0xDD3E, 0x5E71, 0x1397,
    0xA6F5, 0xB968, 0x0000, 0xC12C, 0x4060, 0xE31F, 0x79C8, 0xB6ED,
    0xD4BE, 0x8D46, 0x67D9, 0x724B, 0x94DE, 0x98D4, 0xB0E8, 0x854A,
    0xBB6B, 0xC52A, 0x4FE5, 0xED16, 0x86C5, 0x9AD7, 0x6655, 0x1194,
    0x8ACF, 0xE910, 0x0406, 0xFE81, 0xA0F0, 0x7844, 0x25BA, 0x4BE3,
    0xA2F3, 0x5DFE, 0x80C0, 0x058A, 0x3FAD, 0x21BC, 0x7048, 0xF104,
    0x63DF, 0x77C1, 0xAF75, 0x4263, 0x2030, 0xE51A, 0xFD0E, 0xBF6D,
    0x814C, 0x1814, 0x2635, 0xC32F, 0xBEE1, 0x35A2, 0x88CC, 0x2E39,
    0x9357, 0x55F2, 0xFC82, 0x7A47, 0xC8AC, 0xBAE7, 0x322B, 0xE695,
    0xC0A0, 0x1998, 0x9ED1, 0xA37F, 0x4466, 0x547E, 0x3BAB, 0x0B83,
    0x8CCA, 0xC729, 0x6BD3, 0x283C, 0xA779, 0xBCE2, 0x161D, 0xAD76,
    0xDB3B, 0x6456, 0x744E, 0x141E, 0x92DB, 0x0C0A, 0x486C, 0xB8E4,
    0x9F5D, 0xBD6E, 0x43EF, 0xC4A6, 0x39A8, 0x31A4, 0xD337, 0xF28B,
    0xD532, 0x8B43, 0x6E59, 0xDAB7, 0x018C, 0xB164, 0x9CD2, 0x49E0,
    0xD8B4, 0xACFA, 0xF307, 0xCF25, 0xCAAF, 0xF48E, 0x47E9, 0x1018,
    0x6FD5, 0xF088, 0x4A6F, 0x5C72, 0x3824, 0x57F1, 0x73C7, 0x9751,
    0xCB23, 0xA17C, 0xE89C, 0x3E21, 0x96DD, 0x61DC, 0x0D86, 0x0F85,
    0xE090, 0x7C42, 0x71C4, 0xCCAA, 0x90D8, 0x0605, 0xF701, 0x1C12,
    0xC2A3, 0x6A5F, 0xAEF9, 0x69D0, 0x1791, 0x9958, 0x3A27, 0x27B9,
    0xD938, 0xEB13, 0x2BB3, 0x2233, 0xD2BB, 0xA970, 0x0789, 0x33A7,
    0x2DB6, 0x3C22, 0x1592, 0xC920, 0x8749, 0xAAFF, 0x5078, 0xA57A,
    0x038F, 0x59F8, 0x0980, 0x1A17, 0x65DA, 0xD731, 0x84C6, 0xD0B8,
    0x82C3, 0x29B0, 0x5A77, 0x1E11, 0x7BCB, 0xA8FC, 0x6DD6, 0x2C3A,
];

fn tkip_s(v: u16) -> u16 {
    TKIP_SBOX[(v & 0xff) as usize] ^ TKIP_SBOX[(v >> 8) as usize].swap_bytes()
}

fn mk16(hi: u8, lo: u8) -> u16 {
    (hi as u16) << 8 | lo as u16
}

/// TKIP phase-1 key mixing: the 80-bit TTAK ("P1K") from the TK, the
/// transmitter address and the upper 32 bits of the TSC.
pub fn tkip_phase1(tk: &[u8; 16], ta: &[u8; 6], iv32: u32) -> [u16; 5] {
    let mut p = [iv32 as u16, (iv32 >> 16) as u16, mk16(ta[1], ta[0]), mk16(ta[3], ta[2]), mk16(ta[5], ta[4])];
    for i in 0..8usize {
        let j = 2 * (i & 1);
        p[0] = p[0].wrapping_add(tkip_s(p[4] ^ mk16(tk[1 + j], tk[j])));
        p[1] = p[1].wrapping_add(tkip_s(p[0] ^ mk16(tk[5 + j], tk[4 + j])));
        p[2] = p[2].wrapping_add(tkip_s(p[1] ^ mk16(tk[9 + j], tk[8 + j])));
        p[3] = p[3].wrapping_add(tkip_s(p[2] ^ mk16(tk[13 + j], tk[12 + j])));
        p[4] = p[4].wrapping_add(tkip_s(p[3] ^ mk16(tk[1 + j], tk[j]))).wrapping_add(i as u16);
    }
    p
}

/// TKIP phase-2 key mixing: the per-packet 128-bit RC4 key (WEP seed) from
/// the phase-1 key and the lower 16 bits of the TSC.
pub fn tkip_phase2(tk: &[u8; 16], p1k: &[u16; 5], iv16: u16) -> [u8; 16] {
    let le = |i: usize| u16::from_le_bytes([tk[i], tk[i + 1]]);
    let mut p = [p1k[0], p1k[1], p1k[2], p1k[3], p1k[4], p1k[4].wrapping_add(iv16)];
    p[0] = p[0].wrapping_add(tkip_s(p[5] ^ le(0)));
    p[1] = p[1].wrapping_add(tkip_s(p[0] ^ le(2)));
    p[2] = p[2].wrapping_add(tkip_s(p[1] ^ le(4)));
    p[3] = p[3].wrapping_add(tkip_s(p[2] ^ le(6)));
    p[4] = p[4].wrapping_add(tkip_s(p[3] ^ le(8)));
    p[5] = p[5].wrapping_add(tkip_s(p[4] ^ le(10)));
    p[0] = p[0].wrapping_add((p[5] ^ le(12)).rotate_right(1));
    p[1] = p[1].wrapping_add((p[0] ^ le(14)).rotate_right(1));
    p[2] = p[2].wrapping_add(p[1].rotate_right(1));
    p[3] = p[3].wrapping_add(p[2].rotate_right(1));
    p[4] = p[4].wrapping_add(p[3].rotate_right(1));
    p[5] = p[5].wrapping_add(p[4].rotate_right(1));
    let mut k = [0u8; 16];
    k[0] = (iv16 >> 8) as u8;
    k[1] = ((iv16 >> 8) as u8 | 0x20) & 0x7f;
    k[2] = iv16 as u8;
    k[3] = ((p[5] ^ le(0)) >> 1) as u8;
    for i in 0..6 {
        k[4 + 2 * i..6 + 2 * i].copy_from_slice(&p[i].to_le_bytes());
    }
    k
}

/// RC4 (ARC4) keystream XOR, in place.
pub fn rc4(key: &[u8], data: &mut [u8]) {
    let mut s = [0u8; 256];
    for (i, v) in s.iter_mut().enumerate() {
        *v = i as u8;
    }
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    for b in data.iter_mut() {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        *b ^= s[s[i as usize].wrapping_add(s[j as usize]) as usize];
    }
}

/// IEEE 802.3 CRC-32 (the WEP/TKIP ICV).
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb8_8320 & (c & 1).wrapping_neg());
        }
    }
    !c
}

/// The 48-bit TSC and key index of a TKIP IV/Extended IV header
/// (TSC1 WEPSeed TSC0 keyid|ExtIV TSC2..TSC5); None without the ExtIV bit.
pub fn tkip_tsc(iv: &[u8]) -> Option<(u64, u8)> {
    let h = iv.get(0..8)?;
    if h[3] & 0x20 == 0 {
        return None;
    }
    let tsc = h[2] as u64 | (h[0] as u64) << 8 | (u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64) << 16;
    Some((tsc, h[3] >> 6))
}

/// The TSC of a key RSC field as the EAPOL-Key / GTK KDE carry it
/// (TSC0 first): (iv32, iv16).
pub fn tkip_rsc_split(rsc: &[u8; 8]) -> (u32, u16) {
    (u32::from_le_bytes([rsc[2], rsc[3], rsc[4], rsc[5]]), u16::from_le_bytes([rsc[0], rsc[1]]))
}

/// Decrypt a TKIP MPDU body (ciphertext followed by the 4-byte ICV) in
/// place with phase-1 key `p1k`; true when the ICV verifies.
pub fn tkip_decrypt(tk: &[u8; 16], p1k: &[u16; 5], iv16: u16, body: &mut [u8]) -> bool {
    if body.len() < 4 {
        return false;
    }
    let key = tkip_phase2(tk, p1k, iv16);
    rc4(&key, body);
    let n = body.len() - 4;
    crc32(&body[..n]).to_le_bytes() == body[n..]
}

fn michael_block(l: &mut u32, r: &mut u32) {
    *r ^= l.rotate_left(17);
    *l = l.wrapping_add(*r);
    *r ^= ((*l & 0xff00_ff00) >> 8) | ((*l & 0x00ff_00ff) << 8);
    *l = l.wrapping_add(*r);
    *r ^= l.rotate_left(3);
    *l = l.wrapping_add(*r);
    *r ^= l.rotate_right(2);
    *l = l.wrapping_add(*r);
}

/// Michael over `parts` concatenated (no pseudo header), IEEE 802.11
/// 12.5.2.3: little-endian 32-bit words, padded with 0x5a and zeros.
pub fn michael_raw(key: &[u8; 8], parts: &[&[u8]]) -> [u8; 8] {
    let mut l = u32::from_le_bytes([key[0], key[1], key[2], key[3]]);
    let mut r = u32::from_le_bytes([key[4], key[5], key[6], key[7]]);
    let mut w = [0u8; 4];
    let mut n = 0usize;
    for p in parts {
        for &b in p.iter() {
            w[n] = b;
            n += 1;
            if n == 4 {
                l ^= u32::from_le_bytes(w);
                michael_block(&mut l, &mut r);
                n = 0;
            }
        }
    }
    w[n] = 0x5a;
    for x in w.iter_mut().skip(n + 1) {
        *x = 0;
    }
    l ^= u32::from_le_bytes(w);
    michael_block(&mut l, &mut r);
    michael_block(&mut l, &mut r);
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&l.to_le_bytes());
    out[4..].copy_from_slice(&r.to_le_bytes());
    out
}

/// Michael MIC of an MSDU: pseudo header DA | SA | priority | 0 0 0, then
/// the MSDU payload.
pub fn michael(key: &[u8; 8], da: &[u8], sa: &[u8], priority: u8, payload: &[u8]) -> [u8; 8] {
    michael_raw(key, &[da, sa, &[priority & 0x0f, 0, 0, 0], payload])
}

/// Check the Michael MIC of a received, decrypted TKIP data frame laid out
/// as MAC header (`hdr_len`) | 8-byte IV | payload | MIC (8) | ICV
/// (`icv_len`, 4 when the ICV is still in place, 0 when stripped).
pub fn tkip_frame_mic_ok(rx_mic: &[u8; 8], frame: &[u8], hdr_len: usize, icv_len: usize) -> bool {
    if frame.len() < 24 || frame.len() < hdr_len + 8 + 8 + icv_len {
        return false;
    }
    let fc = u16::from_le_bytes([frame[0], frame[1]]);
    let (to_ds, from_ds) = (fc & 0x0100 != 0, fc & 0x0200 != 0);
    let a = |i: usize| &frame[4 + 6 * i..10 + 6 * i];
    let da = if to_ds { a(2) } else { a(0) };
    let sa = match (to_ds, from_ds) {
        (false, false) | (true, false) => a(1),
        (false, true) => a(2),
        (true, true) => match frame.get(24..30) { Some(x) => x, None => return false },
    };
    // QoS data: priority = TID from the QoS Control field.
    let qos_at = if to_ds && from_ds { 30 } else { 24 };
    let prio = if fc & 0x008c == 0x0088 && hdr_len >= qos_at + 2 { frame[qos_at] & 0x0f } else { 0 };
    let end = frame.len() - icv_len;
    let mic_at = end - 8;
    michael(rx_mic, da, sa, prio, &frame[hdr_len + 8..mic_at])[..] == frame[mic_at..end]
}
