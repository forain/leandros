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
    let mut data = [0u8; 76];
    let (a_lo, a_hi) = if aa <= spa { (aa, spa) } else { (spa, aa) };
    let (n_lo, n_hi) = if anonce <= snonce {
        (anonce, snonce)
    } else {
        (snonce, anonce)
    };
    data[0..6].copy_from_slice(a_lo);
    data[6..12].copy_from_slice(a_hi);
    data[12..44].copy_from_slice(n_lo);
    data[44..76].copy_from_slice(n_hi);
    let mut out = [0u8; 48];
    prf_sha1(pmk, b"Pairwise key expansion", &data, &mut out);
    let mut ptk = Ptk {
        kck: [0; 16],
        kek: [0; 16],
        tk: [0; 16],
    };
    ptk.kck.copy_from_slice(&out[0..16]);
    ptk.kek.copy_from_slice(&out[16..32]);
    ptk.tk.copy_from_slice(&out[32..48]);
    ptk
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

/// MIC for key descriptor version 2: HMAC-SHA1(KCK, PDU with the MIC field
/// zeroed), truncated to 16 bytes.
fn compute_mic(kck: &[u8; 16], pdu: &[u8]) -> Option<[u8; 16]> {
    let total = eapol_pdu_len(pdu)?;
    let pdu = &pdu[..total];
    let full = hmac_sha1(
        kck,
        &[&pdu[..MIC_OFFSET], &[0u8; 16], &pdu[MIC_OFFSET + 16..]],
    );
    let mut mic = [0u8; 16];
    mic.copy_from_slice(&full[..16]);
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
    [
        0x30, 0x14, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01,
        0x00, 0x00, 0x0f, 0xac, 0x02, 0x00, 0x00,
    ]
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
    /// Install a group key (CCMP GTK) with its key index and receive
    /// sequence counter.
    InstallGtk {
        key_id: u8,
        key: Vec<u8>,
        rsc: [u8; 8],
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
        }
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
        if k.key_info & key_info::VERSION_MASK != 2 {
            return Self::err("unsupported key descriptor version");
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
        let ptk = derive_ptk(&self.pmk, &self.ap_addr, &self.own_addr, &k.nonce, &self.snonce);
        self.anonce = Some(k.nonce);
        self.replay = Some(u64::from_be_bytes(k.replay_counter));
        let mut msg2 = build_eapol_key(
            k.version,
            2 | key_info::TYPE_PAIRWISE | key_info::MIC,
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
        if gtk.key.len() != GTK_LEN {
            return Self::err("unsupported GTK length");
        }
        self.replay = Some(u64::from_be_bytes(k.replay_counter));

        let mut msg4 = build_eapol_key(
            k.version,
            2 | key_info::TYPE_PAIRWISE | key_info::MIC | key_info::SECURE,
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
        if gtk.key.len() != GTK_LEN {
            return Self::err("unsupported GTK length");
        }
        self.replay = Some(u64::from_be_bytes(k.replay_counter));
        let mut reply = build_eapol_key(
            k.version,
            2 | key_info::MIC | key_info::SECURE,
            0,
            &k.replay_counter,
            &[0u8; 32],
            &[],
        );
        eapol_set_mic(&ptk.kck, &mut reply);
        vec![
            HsEvent::Send(reply),
            HsEvent::InstallGtk {
                key_id: gtk.key_id,
                key: gtk.key,
                rsc: k.rsc,
            },
        ]
    }
}
