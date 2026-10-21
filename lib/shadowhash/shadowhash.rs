//! Shadow password hashing, shared by login, sshd and the greetd PAM module.
//!
//! One source file, included with `#[path = "<rel>/lib/shadowhash/shadowhash.rs"] mod
//! shadowhash;`. Dependency-free, no alloc, no_std-compatible.
//!
//! Formats accepted by [`verify`]:
//!   * `$6$[rounds=N$]<salt>$<86 chars>`  sha512-crypt (Drepper), default
//!     5000 rounds, rounds clamped to 1000..=999_999_999, salt cut at 16.
//!   * `$sha256$<salt>$<hex>`  legacy: hex = sha256(salt ++ password).
//!   * empty field: passwordless (login/PAM behaviour; sshd refuses it itself).
#![allow(dead_code)]

const K512: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
    0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
    0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
    0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30,
    0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
    0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
    0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];
const H512: [u64; 8] = [
    0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1,
    0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179,
];
const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
    0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
    0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];
const H256: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
    0x1f83d9ab, 0x5be0cd19,
];

const MAX_PW: usize = 256;
const MAX_SALT: usize = 16;
const DEFAULT_ROUNDS: u64 = 5000;
const B64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Number of bytes of the encoded sha512-crypt digest.
pub const CRYPT_LEN: usize = 86;

struct Sha512 {
    h: [u64; 8],
    buf: [u8; 128],
    n: usize,
    total: u128,
}

impl Sha512 {
    fn new() -> Self {
        Sha512 { h: H512, buf: [0; 128], n: 0, total: 0 }
    }
    fn block(h: &mut [u64; 8], b: &[u8]) {
        let mut w = [0u64; 80];
        for i in 0..16 {
            let mut v = 0u64;
            for j in 0..8 {
                v = (v << 8) | b[i * 8 + j] as u64;
            }
            w[i] = v;
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = *h;
        for i in 0..80 {
            let s1 = v[4].rotate_right(14) ^ v[4].rotate_right(18) ^ v[4].rotate_right(41);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K512[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(28) ^ v[0].rotate_right(34) ^ v[0].rotate_right(39);
            let mj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(mj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    fn update(&mut self, mut d: &[u8]) {
        while !d.is_empty() {
            let take = core::cmp::min(128 - self.n, d.len());
            self.buf[self.n..self.n + take].copy_from_slice(&d[..take]);
            self.n += take;
            d = &d[take..];
            if self.n == 128 {
                let b = self.buf;
                Self::block(&mut self.h, &b);
                self.n = 0;
            }
        }
    }
    fn write(&mut self, d: &[u8]) {
        self.total += d.len() as u128;
        self.update(d);
    }
    fn finish(mut self) -> [u8; 64] {
        let bits = self.total * 8;
        self.update(&[0x80]);
        while self.n != 112 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 64];
        for i in 0..8 {
            out[i * 8..i * 8 + 8].copy_from_slice(&self.h[i].to_be_bytes());
        }
        out
    }
}

struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    n: usize,
    total: u64,
}

impl Sha256 {
    fn new() -> Self {
        Sha256 { h: H256, buf: [0; 64], n: 0, total: 0 }
    }
    fn block(h: &mut [u32; 8], b: &[u8]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = *h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K256[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let mj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(mj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    fn update(&mut self, mut d: &[u8]) {
        while !d.is_empty() {
            let take = core::cmp::min(64 - self.n, d.len());
            self.buf[self.n..self.n + take].copy_from_slice(&d[..take]);
            self.n += take;
            d = &d[take..];
            if self.n == 64 {
                let b = self.buf;
                Self::block(&mut self.h, &b);
                self.n = 0;
            }
        }
    }
    fn write(&mut self, d: &[u8]) {
        self.total += d.len() as u64;
        self.update(d);
    }
    fn finish(mut self) -> [u8; 32] {
        let bits = self.total * 8;
        self.update(&[0x80]);
        while self.n != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for i in 0..8 {
            out[i * 4..i * 4 + 4].copy_from_slice(&self.h[i].to_be_bytes());
        }
        out
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Feed `len` bytes of `src` repeated cyclically into `h`.
fn feed_cycled(h: &mut Sha512, src: &[u8], len: usize) {
    let mut left = len;
    while left >= src.len() {
        h.write(src);
        left -= src.len();
    }
    h.write(&src[..left]);
}

/// sha512-crypt digest of `password` with `salt` (cut to 16 bytes) and
/// `rounds` (used as given; callers clamp), as 86 crypt-base64 characters.
/// Returns None when the password exceeds 256 bytes.
pub fn sha512_crypt_raw(password: &[u8], salt: &[u8], rounds: u64) -> Option<[u8; CRYPT_LEN]> {
    let salt = &salt[..core::cmp::min(salt.len(), MAX_SALT)];
    let pl = password.len();
    if pl > MAX_PW {
        return None;
    }
    let mut hb = Sha512::new();
    hb.write(password);
    hb.write(salt);
    hb.write(password);
    let b = hb.finish();

    let mut ha = Sha512::new();
    ha.write(password);
    ha.write(salt);
    feed_cycled(&mut ha, &b, pl);
    let mut cnt = pl;
    while cnt > 0 {
        if cnt & 1 != 0 {
            ha.write(&b);
        } else {
            ha.write(password);
        }
        cnt >>= 1;
    }
    let mut a = ha.finish();

    let mut hp = Sha512::new();
    for _ in 0..pl {
        hp.write(password);
    }
    let dp = hp.finish();
    let mut pbuf = [0u8; MAX_PW];
    for i in 0..pl {
        pbuf[i] = dp[i % 64];
    }

    let mut hs = Sha512::new();
    for _ in 0..(16 + a[0] as usize) {
        hs.write(salt);
    }
    let ds = hs.finish();
    let mut sbuf = [0u8; MAX_SALT];
    for i in 0..salt.len() {
        sbuf[i] = ds[i % 64];
    }
    let (p, s) = (&pbuf[..pl], &sbuf[..salt.len()]);

    for i in 0..rounds {
        let mut c = Sha512::new();
        if i & 1 != 0 {
            c.write(p);
        } else {
            c.write(&a);
        }
        if i % 3 != 0 {
            c.write(s);
        }
        if i % 7 != 0 {
            c.write(p);
        }
        if i & 1 != 0 {
            c.write(&a);
        } else {
            c.write(p);
        }
        a = c.finish();
    }

    const ORDER: [(usize, usize, usize); 21] = [
        (0, 21, 42), (22, 43, 1), (44, 2, 23), (3, 24, 45), (25, 46, 4), (47, 5, 26),
        (6, 27, 48), (28, 49, 7), (50, 8, 29), (9, 30, 51), (31, 52, 10), (53, 11, 32),
        (12, 33, 54), (34, 55, 13), (56, 14, 35), (15, 36, 57), (37, 58, 16), (59, 17, 38),
        (18, 39, 60), (40, 61, 19), (62, 20, 41),
    ];
    let mut out = [0u8; CRYPT_LEN];
    let mut o = 0;
    for (x, y, z) in ORDER {
        let mut w = ((a[x] as u32) << 16) | ((a[y] as u32) << 8) | a[z] as u32;
        for _ in 0..4 {
            out[o] = B64[(w & 63) as usize];
            o += 1;
            w >>= 6;
        }
    }
    let mut w = a[63] as u32;
    for _ in 0..2 {
        out[o] = B64[(w & 63) as usize];
        o += 1;
        w >>= 6;
    }
    Some(out)
}

fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 12 {
        return None;
    }
    let mut v = 0u64;
    for c in s.bytes() {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u64;
    }
    Some(v)
}

fn verify_sha512(rest: &str, password: &[u8]) -> bool {
    let mut rounds = DEFAULT_ROUNDS;
    let mut rest = rest;
    if let Some(r) = rest.strip_prefix("rounds=") {
        let Some((n, tail)) = r.split_once('$') else { return false };
        let Some(n) = parse_u64(n) else { return false };
        rounds = n.clamp(1000, 999_999_999);
        rest = tail;
    }
    let Some((salt, hash)) = rest.split_once('$') else { return false };
    if hash.len() != CRYPT_LEN || salt.is_empty() {
        return false;
    }
    match sha512_crypt_raw(password, salt.as_bytes(), rounds) {
        Some(out) => ct_eq(&out, hash.as_bytes()),
        None => false,
    }
}

fn verify_sha256_legacy(rest: &str, password: &[u8]) -> bool {
    let Some((salt, hex)) = rest.split_once('$') else { return false };
    if hex.len() != 64 {
        return false;
    }
    let mut h = Sha256::new();
    h.write(salt.as_bytes());
    h.write(password);
    let d = h.finish();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut want = [0u8; 64];
    for i in 0..32 {
        want[2 * i] = HEX[(d[i] >> 4) as usize];
        want[2 * i + 1] = HEX[(d[i] & 15) as usize];
    }
    ct_eq(&want, hex.as_bytes())
}

/// Check `password` against a /etc/shadow hash field. An empty field returns
/// true (passwordless); callers that refuse empty passwords must check first.
pub fn verify(hash_field: &str, password: &[u8]) -> bool {
    if hash_field.is_empty() {
        return true;
    }
    if let Some(r) = hash_field.strip_prefix("$6$") {
        return verify_sha512(r, password);
    }
    if let Some(r) = hash_field.strip_prefix("$sha256$") {
        return verify_sha256_legacy(r, password);
    }
    false
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    const V1: &str = "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1";
    const V2: &str = "$6$rounds=10000$saltstringsaltst$OW1/O6BYHV6BcXZu8QVeXbDWra3Oeqh0sbHbbMCVNSnCM/UrjmM0Dp8vOuZeHBy/YTBmSK6H9qs/y3RnOaw5v.";

    #[test]
    fn spec_vectors() {
        assert!(verify(V1, b"Hello world!"));
        assert!(!verify(V1, b"Hello world"));
        assert!(verify(V2, b"Hello world!"));
        // untruncated salt in the field still verifies (cut at 16)
        assert!(verify(
            "$6$rounds=10000$saltstringsaltstring$OW1/O6BYHV6BcXZu8QVeXbDWra3Oeqh0sbHbbMCVNSnCM/UrjmM0Dp8vOuZeHBy/YTBmSK6H9qs/y3RnOaw5v.",
            b"Hello world!"
        ));
    }

    #[test]
    fn legacy() {
        let f = "$sha256$lnd0$777a310c8707848af9b04df014e11fe313c7603650c649862d13b53553bf9875";
        assert!(verify(f, b"root"));
        assert!(!verify(f, b"x"));
        assert!(!verify("$sha256$salt$", b"anything"));
        assert!(verify("", b"x"));
        assert!(!verify("garbage", b"x"));
    }

    #[test]
    fn python_generated() {
        // Output of scripts/mkfs-f2fs-populated.py shadow_hash().
        assert!(verify("$6$rounds=65536$lnd0rootsalt0000$e/pKi.KtaNXG2AESzxZESa5yppf.wW2fmfF6I0uGSkM9OzFob1CXV0MQvpPb2zsW7fvn9qqB3MEWvo9F5q3.K/", b"root"));
        assert!(!verify("$6$rounds=65536$lnd0rootsalt0000$e/pKi.KtaNXG2AESzxZESa5yppf.wW2fmfF6I0uGSkM9OzFob1CXV0MQvpPb2zsW7fvn9qqB3MEWvo9F5q3.K/", b"roo"));
        assert!(verify("$6$rounds=65536$lnd0leandrosalt1$DQ11AaldqWyE98K3fN.TSIT2.3Ak2LwBci2bWJGtEvVZlCOjLJEdR41JYBW9QFF9kyt3AAGYNu5s64mz9AM/Y1", b"leandro"));
    }
}
