//! WPA3-Personal: SAE (Simultaneous Authentication of Equals, IEEE 802.11-2020
//! 12.4) on NIST P-256 (group 19), the dragonfly handshake both roles use.
//!
//! * Password element: hunt-and-peck (12.4.4.2.2) or hash-to-element
//!   (H2E, 12.4.4.2.3: SSWU of RFC 9380 section 6.6.2 per u1/u2, PT = P1 + P2,
//!   PWE = val * PT with val from the two MAC addresses).
//! * Commit (scalar, element), shared secret K, KCK / PMK / PMKID, confirm.
//! * Anti-clogging token helpers and the auth-frame body encodings.
//!
//! Everything is hand-rolled, like `wifi_crypto`: kernel code may not touch
//! SIMD registers. Field arithmetic is 4x64-bit Montgomery. NOT constant
//! time (variable-time scalar multiplication, hunt-and-peck loop): fine for
//! the simulator and for H2E clients, but a hardened hunt-and-peck AP would
//! need more; this station prefers H2E whenever the AP offers it.
//!
//! Known-answer tests live in `selftest()`, run on the host
//! (`drivers/iwlwifi-tests`) and at boot by `wifi_sim`.

use alloc::vec::Vec;

use crate::wifi_crypto as wc;

pub const GROUP: u16 = 19;
/// Auth algorithm number for SAE.
pub const AUTH_ALG_SAE: u16 = 3;
/// Status codes.
pub const STATUS_SUCCESS: u16 = 0;
pub const STATUS_ANTI_CLOGGING: u16 = 76;
pub const STATUS_H2E: u16 = 126;
pub const STATUS_UNSUPPORTED_GROUP: u16 = 77;
pub const STATUS_CONFIRM_MISMATCH: u16 = 1;

type U256 = [u64; 4];

const P: U256 = [0xffff_ffff_ffff_ffff, 0x0000_0000_ffff_ffff, 0x0000_0000_0000_0000, 0xffff_ffff_0000_0001];
const N: U256 = [0xf3b9_cac2_fc63_2551, 0xbce6_faad_a717_9e84, 0xffff_ffff_ffff_ffff, 0xffff_ffff_0000_0000];
const B: U256 = [0x3bce_3c3e_27d2_604b, 0x651d_06b0_cc53_b0f6, 0xb3eb_bd55_7698_86bc, 0x5ac6_35d8_aa3a_93e7];
const GX: U256 = [0xf4a1_3945_d898_c296, 0x7703_7d81_2deb_33a0, 0xf8bc_e6e5_63a4_40f2, 0x6b17_d1f2_e12c_4247];
const GY: U256 = [0xcbb6_4068_37bf_51f5, 0x2bce_3357_6b31_5ece, 0x8ee7_eb4a_7c0f_9e16, 0x4fe3_42e2_fe1a_7f9b];

fn from_be(b: &[u8]) -> U256 {
    let mut o = [0u64; 4];
    for i in 0..32.min(b.len()) {
        let byte = b[b.len() - 1 - i];
        o[i / 8] |= (byte as u64) << (8 * (i % 8));
    }
    o
}
fn to_be(a: &U256) -> [u8; 32] {
    let mut o = [0u8; 32];
    for i in 0..32 { o[31 - i] = (a[i / 8] >> (8 * (i % 8))) as u8; }
    o
}
fn is_zero(a: &U256) -> bool { a.iter().all(|&x| x == 0) }
fn ge(a: &U256, b: &U256) -> bool {
    for i in (0..4).rev() { if a[i] != b[i] { return a[i] > b[i]; } }
    true
}
fn add_raw(a: &U256, b: &U256) -> (U256, bool) {
    let mut o = [0u64; 4];
    let mut c = 0u128;
    for i in 0..4 { let v = a[i] as u128 + b[i] as u128 + c; o[i] = v as u64; c = v >> 64; }
    (o, c != 0)
}
fn sub_raw(a: &U256, b: &U256) -> (U256, bool) {
    let mut o = [0u64; 4];
    let mut br = 0i128;
    for i in 0..4 {
        let v = a[i] as i128 - b[i] as i128 - br;
        o[i] = v as u64;
        br = if v < 0 { 1 } else { 0 };
    }
    (o, br != 0)
}
/// (a + b) mod m for a, b < m.
fn add_mod(a: &U256, b: &U256, m: &U256) -> U256 {
    let (s, c) = add_raw(a, b);
    if c || ge(&s, m) { sub_raw(&s, m).0 } else { s }
}
fn sub_mod(a: &U256, b: &U256, m: &U256) -> U256 {
    let (d, br) = sub_raw(a, b);
    if br { add_raw(&d, m).0 } else { d }
}
/// Reduce a big-endian integer of any length modulo `m` (bitwise).
fn reduce_be(bytes: &[u8], m: &U256) -> U256 {
    let mut r = [0u64; 4];
    for &byte in bytes {
        for bit in (0..8).rev() {
            r = add_mod(&r, &r, m);
            if (byte >> bit) & 1 == 1 { r = add_mod(&r, &[1, 0, 0, 0], m); }
        }
    }
    r
}

/// Montgomery arithmetic modulo the odd 256-bit prime `m`.
struct Mont { m: U256, n0: u64, r2: U256, one: U256 }

impl Mont {
    fn new(m: U256) -> Mont {
        let mut inv = 1u64;
        for _ in 0..7 { inv = inv.wrapping_mul(2u64.wrapping_sub(m[0].wrapping_mul(inv))); }
        let n0 = inv.wrapping_neg();
        // R^2 mod m: 1 doubled 512 times.
        let mut x = [1u64, 0, 0, 0];
        for _ in 0..512 { x = add_mod(&x, &x, &m); }
        let mut mt = Mont { m, n0, r2: x, one: [0; 4] };
        mt.one = mt.mul(&[1, 0, 0, 0], &mt.r2.clone());
        mt
    }
    fn mul(&self, a: &U256, b: &U256) -> U256 {
        let mut t = [0u64; 6];
        for i in 0..4 {
            let mut c = 0u128;
            for j in 0..4 {
                let v = t[j] as u128 + (a[j] as u128) * (b[i] as u128) + c;
                t[j] = v as u64;
                c = v >> 64;
            }
            let v = t[4] as u128 + c;
            t[4] = v as u64;
            t[5] = (v >> 64) as u64;
            let mq = t[0].wrapping_mul(self.n0);
            let mut c = (t[0] as u128 + (mq as u128) * (self.m[0] as u128)) >> 64;
            for j in 1..4 {
                let v = t[j] as u128 + (mq as u128) * (self.m[j] as u128) + c;
                t[j - 1] = v as u64;
                c = v >> 64;
            }
            let v = t[4] as u128 + c;
            t[3] = v as u64;
            let v2 = t[5] as u128 + (v >> 64);
            t[4] = v2 as u64;
            t[5] = 0;
        }
        let r = [t[0], t[1], t[2], t[3]];
        if t[4] != 0 || ge(&r, &self.m) { sub_raw(&r, &self.m).0 } else { r }
    }
    fn to(&self, a: &U256) -> U256 { self.mul(a, &self.r2) }
    fn from(&self, a: &U256) -> U256 { self.mul(a, &[1, 0, 0, 0]) }
    fn add(&self, a: &U256, b: &U256) -> U256 { add_mod(a, b, &self.m) }
    fn sub(&self, a: &U256, b: &U256) -> U256 { sub_mod(a, b, &self.m) }
    fn neg(&self, a: &U256) -> U256 { if is_zero(a) { *a } else { sub_raw(&self.m, a).0 } }
    fn sqr(&self, a: &U256) -> U256 { self.mul(a, a) }
    /// a^e, e a plain integer.
    fn pow(&self, a: &U256, e: &U256) -> U256 {
        let mut r = self.one;
        for i in (0..256).rev() {
            r = self.sqr(&r);
            if (e[i / 64] >> (i % 64)) & 1 == 1 { r = self.mul(&r, a); }
        }
        r
    }
    fn inv(&self, a: &U256) -> U256 { self.pow(a, &sub_raw(&self.m, &[2, 0, 0, 0]).0) }
}

/// A point in Jacobian coordinates, Montgomery form; z == 0 is infinity.
#[derive(Clone, Copy)]
struct Jac { x: U256, y: U256, z: U256 }

/// An affine point (plain integers) or infinity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pt { pub x: [u8; 32], pub y: [u8; 32] }

struct Curve { f: Mont, b: U256, three: U256 }

impl Curve {
    fn new() -> Curve {
        let f = Mont::new(P);
        let b = f.to(&B);
        let three = f.to(&[3, 0, 0, 0]);
        Curve { f, b, three }
    }
    fn inf(&self) -> Jac { Jac { x: self.f.one, y: self.f.one, z: [0; 4] } }
    fn is_inf(&self, p: &Jac) -> bool { is_zero(&p.z) }
    fn lift(&self, p: &Pt) -> Jac {
        Jac { x: self.f.to(&from_be(&p.x)), y: self.f.to(&from_be(&p.y)), z: self.f.one }
    }
    fn affine(&self, p: &Jac) -> Option<Pt> {
        if self.is_inf(p) { return None; }
        let zi = self.f.inv(&p.z);
        let zi2 = self.f.sqr(&zi);
        let zi3 = self.f.mul(&zi2, &zi);
        Some(Pt { x: to_be(&self.f.from(&self.f.mul(&p.x, &zi2))), y: to_be(&self.f.from(&self.f.mul(&p.y, &zi3))) })
    }
    fn dbl(&self, p: &Jac) -> Jac {
        let f = &self.f;
        if self.is_inf(p) || is_zero(&p.y) { return self.inf(); }
        let delta = f.sqr(&p.z);
        let gamma = f.sqr(&p.y);
        let beta = f.mul(&p.x, &gamma);
        let t = f.mul(&f.sub(&p.x, &delta), &f.add(&p.x, &delta));
        let alpha = f.mul(&self.three, &t);
        let beta4 = f.add(&f.add(&beta, &beta), &f.add(&beta, &beta));
        let beta8 = f.add(&beta4, &beta4);
        let x3 = f.sub(&f.sqr(&alpha), &beta8);
        let yz = f.add(&p.y, &p.z);
        let z3 = f.sub(&f.sub(&f.sqr(&yz), &gamma), &delta);
        let g2 = f.sqr(&gamma);
        let g2_2 = f.add(&g2, &g2);
        let g2_4 = f.add(&g2_2, &g2_2);
        let g2_8 = f.add(&g2_4, &g2_4);
        let y3 = f.sub(&f.mul(&alpha, &f.sub(&beta4, &x3)), &g2_8);
        Jac { x: x3, y: y3, z: z3 }
    }
    fn add(&self, p: &Jac, q: &Jac) -> Jac {
        let f = &self.f;
        if self.is_inf(p) { return *q; }
        if self.is_inf(q) { return *p; }
        let z1z1 = f.sqr(&p.z);
        let z2z2 = f.sqr(&q.z);
        let u1 = f.mul(&p.x, &z2z2);
        let u2 = f.mul(&q.x, &z1z1);
        let s1 = f.mul(&f.mul(&p.y, &q.z), &z2z2);
        let s2 = f.mul(&f.mul(&q.y, &p.z), &z1z1);
        if u1 == u2 {
            return if s1 == s2 { self.dbl(p) } else { self.inf() };
        }
        let h = f.sub(&u2, &u1);
        let h2 = f.add(&h, &h);
        let i = f.sqr(&h2);
        let j = f.mul(&h, &i);
        let r0 = f.sub(&s2, &s1);
        let r = f.add(&r0, &r0);
        let v = f.mul(&u1, &i);
        let x3 = f.sub(&f.sub(&f.sqr(&r), &j), &f.add(&v, &v));
        let s1j = f.mul(&s1, &j);
        let y3 = f.sub(&f.mul(&r, &f.sub(&v, &x3)), &f.add(&s1j, &s1j));
        let zs = f.sqr(&f.add(&p.z, &q.z));
        let z3 = f.mul(&f.sub(&f.sub(&zs, &z1z1), &z2z2), &h);
        Jac { x: x3, y: y3, z: z3 }
    }
    fn mul(&self, k: &U256, p: &Jac) -> Jac {
        let mut r = self.inf();
        for i in (0..256).rev() {
            r = self.dbl(&r);
            if (k[i / 64] >> (i % 64)) & 1 == 1 { r = self.add(&r, p); }
        }
        r
    }
    fn on_curve(&self, p: &Pt) -> bool {
        let f = &self.f;
        let (x, y) = (from_be(&p.x), from_be(&p.y));
        if ge(&x, &P) || ge(&y, &P) { return false; }
        let (x, y) = (f.to(&x), f.to(&y));
        let rhs = f.add(&f.sub(&f.mul(&f.sqr(&x), &x), &f.mul(&self.three, &x)), &self.b);
        f.sqr(&y) == rhs
    }
    fn generator(&self) -> Jac { self.lift(&Pt { x: to_be(&GX), y: to_be(&GY) }) }
    /// g(x) = x^3 - 3x + b.
    fn rhs(&self, x: &U256) -> U256 {
        let f = &self.f;
        f.add(&f.sub(&f.mul(&f.sqr(x), x), &f.mul(&self.three, x)), &self.b)
    }
    fn is_square(&self, a: &U256) -> bool {
        let e = shr1(&sub_raw(&P, &[1, 0, 0, 0]).0);
        let r = self.f.pow(a, &e);
        r == self.f.one || is_zero(&r)
    }
    /// sqrt for p = 3 mod 4: a^((p+1)/4).
    fn sqrt(&self, a: &U256) -> U256 {
        let e = shr2(&add_raw(&P, &[1, 0, 0, 0]).0);
        self.f.pow(a, &e)
    }
}
fn shr1(a: &U256) -> U256 {
    let mut o = [0u64; 4];
    for i in 0..4 { o[i] = (a[i] >> 1) | if i < 3 { a[i + 1] << 63 } else { 0 }; }
    o
}
fn shr2(a: &U256) -> U256 { shr1(&shr1(a)) }

/// k * G for a big-endian 32-byte scalar (test helper).
pub fn base_mul(k: &[u8; 32]) -> Option<Pt> {
    let c = Curve::new();
    c.affine(&c.mul(&from_be(k), &c.generator()))
}

/// RFC 9380 6.6.2 simple SWU map for P-256 (Z = -10) of `u` (< p).
fn sswu(c: &Curve, u: &U256) -> Pt {
    let f = &c.f;
    let u = f.to(u);
    let z = f.neg(&f.to(&[10, 0, 0, 0]));
    let a = f.neg(&c.three);
    let u2 = f.sqr(&u);
    let zu2 = f.mul(&z, &u2);
    let tv1 = f.add(&f.sqr(&zu2), &zu2);
    let tv1i = if is_zero(&tv1) { [0; 4] } else { f.inv(&tv1) };
    let x1 = if is_zero(&tv1i) {
        f.mul(&c.b, &f.inv(&f.mul(&z, &a)))
    } else {
        f.mul(&f.mul(&f.neg(&c.b), &f.inv(&a)), &f.add(&f.one, &tv1i))
    };
    let gx1 = c.rhs(&x1);
    let (x, mut y) = if c.is_square(&gx1) {
        (x1, c.sqrt(&gx1))
    } else {
        let x2 = f.mul(&zu2, &x1);
        (x2, c.sqrt(&c.rhs(&x2)))
    };
    let su = f.from(&u)[0] & 1;
    if su != f.from(&y)[0] & 1 { y = f.neg(&y); }
    Pt { x: to_be(&f.from(&x)), y: to_be(&f.from(&y)) }
}

/// RFC 9380 map_to_curve (SSWU) for a 32-byte big-endian field element.
pub fn map_to_curve(u: &[u8; 32]) -> Pt { sswu(&Curve::new(), &from_be(u)) }

// ── password element ────────────────────────────────────────────────────────

/// Hunt-and-peck PWE (802.11-2020 12.4.4.2.2) for group 19.
pub fn pwe_hunt_and_peck(password: &[u8], mac_a: &[u8; 6], mac_b: &[u8; 6], identifier: Option<&[u8]>) -> Pt {
    let c = Curve::new();
    let (hi, lo) = if mac_a >= mac_b { (mac_a, mac_b) } else { (mac_b, mac_a) };
    let mut key = [0u8; 12];
    key[..6].copy_from_slice(hi);
    key[6..].copy_from_slice(lo);
    let p_be = to_be(&P);
    let mut found: Option<(U256, U256, u8)> = None;
    let mut counter: u32 = 1;
    while counter <= 40 || found.is_none() {
        let ctr = [counter as u8];
        let seed = match identifier {
            Some(id) => wc::hmac_sha256(&key, &[password, id, &ctr]),
            None => wc::hmac_sha256(&key, &[password, &ctr]),
        };
        let mut val = [0u8; 32];
        wc::kdf_sha256(&seed, b"SAE Hunting and Pecking", &p_be, &mut val);
        let x = from_be(&val);
        if found.is_none() && !ge(&x, &P) {
            let xm = c.f.to(&x);
            let g = c.rhs(&xm);
            if c.is_square(&g) {
                let y = c.f.from(&c.sqrt(&g));
                found = Some((x, y, seed[31] & 1));
            }
        }
        counter += 1;
        if counter > 1000 { break; }
    }
    let (x, y, lsb_seed) = found.expect("no PWE");
    let y = if (y[0] & 1) as u8 == lsb_seed { y } else { sub_raw(&P, &y).0 };
    Pt { x: to_be(&x), y: to_be(&y) }
}

/// H2E: the password-to-element PT (802.11-2020 12.4.4.2.3).
pub fn pt_h2e(ssid: &[u8], password: &[u8], identifier: Option<&[u8]>) -> Pt {
    let c = Curve::new();
    let seed = match identifier {
        Some(id) => wc::hmac_sha256(ssid, &[password, id]),
        None => wc::hmac_sha256(ssid, &[password]),
    };
    let mut v1 = [0u8; 48];
    wc::kdf_sha256(&seed, b"SAE Hash to Element u1 P1", &[], &mut v1);
    let mut v2 = [0u8; 48];
    wc::kdf_sha256(&seed, b"SAE Hash to Element u2 P2", &[], &mut v2);
    let p1 = sswu(&c, &reduce_be(&v1, &P));
    let p2 = sswu(&c, &reduce_be(&v2, &P));
    c.affine(&c.add(&c.lift(&p1), &c.lift(&p2))).expect("PT at infinity")
}

/// H2E: PWE = (val mod (r-1) + 1) * PT with val = HMAC-SHA256(0^32, max(a,b) || min(a,b)).
pub fn pwe_from_pt(pt: &Pt, mac_a: &[u8; 6], mac_b: &[u8; 6]) -> Pt {
    let c = Curve::new();
    let (hi, lo) = if mac_a >= mac_b { (mac_a, mac_b) } else { (mac_b, mac_a) };
    let val = from_be(&wc::hmac_sha256(&[0u8; 32], &[hi, lo]));
    let nm1 = sub_raw(&N, &[1, 0, 0, 0]).0;
    let mut v = val;
    while ge(&v, &nm1) { v = sub_raw(&v, &nm1).0; }
    let k = add_raw(&v, &[1, 0, 0, 0]).0;
    c.affine(&c.mul(&k, &c.lift(pt))).expect("PWE at infinity")
}

// ── the exchange ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SaeErr {
    Malformed,
    BadScalar,
    BadElement,
    Reflection,
    BadConfirm,
    NoSecret,
}

/// One side of an SAE exchange.
pub struct Sae {
    pwe: Pt,
    rand: U256,
    pub scalar: [u8; 32],
    pub element: Pt,
    peer: Option<([u8; 32], Pt)>,
    pub kck: [u8; 32],
    pub pmk: [u8; 32],
    pub pmkid: [u8; 16],
    pub send_confirm: u16,
    keyed: bool,
}

fn random_scalar(rng: &mut dyn FnMut(&mut [u8])) -> U256 {
    loop {
        let mut b = [0u8; 32];
        rng(&mut b);
        let v = from_be(&b);
        if !is_zero(&v) && !ge(&v, &N) { return v; }
    }
}

impl Sae {
    /// Generate our commit for `pwe`.
    pub fn new(pwe: Pt, rng: &mut dyn FnMut(&mut [u8])) -> Sae {
        let c = Curve::new();
        let rand = random_scalar(rng);
        let mask = random_scalar(rng);
        let scalar = add_mod(&rand, &mask, &N);
        let mp = c.affine(&c.mul(&mask, &c.lift(&pwe))).expect("mask*PWE");
        // ELEMENT = inverse(mask * PWE).
        let ny = sub_raw(&P, &from_be(&mp.y)).0;
        Sae {
            pwe, rand, scalar: to_be(&scalar), element: Pt { x: mp.x, y: to_be(&ny) },
            peer: None, kck: [0; 32], pmk: [0; 32], pmkid: [0; 16], send_confirm: 1, keyed: false,
        }
    }

    /// The commit body after the group id: scalar || element x || element y.
    pub fn commit_body(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(96);
        v.extend_from_slice(&self.scalar);
        v.extend_from_slice(&self.element.x);
        v.extend_from_slice(&self.element.y);
        v
    }

    /// Process the peer's commit (the 96 bytes after the group id and token);
    /// derives KCK / PMK / PMKID.
    pub fn process_commit(&mut self, body: &[u8]) -> Result<(), SaeErr> {
        if body.len() < 96 { return Err(SaeErr::Malformed); }
        let c = Curve::new();
        let mut ps = [0u8; 32];
        ps.copy_from_slice(&body[..32]);
        let pe = Pt { x: body[32..64].try_into().unwrap(), y: body[64..96].try_into().unwrap() };
        let s = from_be(&ps);
        if is_zero(&s) || ge(&s, &N) { return Err(SaeErr::BadScalar); }
        if !c.on_curve(&pe) { return Err(SaeErr::BadElement); }
        if ps == self.scalar && pe == self.element { return Err(SaeErr::Reflection); }
        // K = rand * (peer_scalar * PWE + PEER_ELEMENT)
        let t = c.add(&c.mul(&s, &c.lift(&self.pwe)), &c.lift(&pe));
        let k = c.affine(&c.mul(&self.rand, &t)).ok_or(SaeErr::BadElement)?;
        let keyseed = wc::hmac_sha256(&[0u8; 32], &[&k.x]);
        let sum = to_be(&add_mod(&from_be(&self.scalar), &s, &N));
        let mut out = [0u8; 64];
        wc::kdf_sha256(&keyseed, b"SAE KCK and PMK", &sum, &mut out);
        self.kck.copy_from_slice(&out[..32]);
        self.pmk.copy_from_slice(&out[32..]);
        self.pmkid.copy_from_slice(&sum[..16]);
        self.peer = Some((ps, pe));
        self.keyed = true;
        Ok(())
    }

    fn confirm_for(&self, sc: u16, s1: &[u8; 32], e1: &Pt, s2: &[u8; 32], e2: &Pt) -> [u8; 32] {
        wc::hmac_sha256(&self.kck, &[&sc.to_le_bytes(), s1, &e1.x, &e1.y, s2, &e2.x, &e2.y])
    }

    /// Our confirm body: send-confirm (LE) || confirm.
    pub fn confirm_body(&self) -> Result<Vec<u8>, SaeErr> {
        let (ps, pe) = self.peer.as_ref().ok_or(SaeErr::NoSecret)?;
        let mut v = Vec::with_capacity(34);
        v.extend_from_slice(&self.send_confirm.to_le_bytes());
        v.extend_from_slice(&self.confirm_for(self.send_confirm, &self.scalar, &self.element, ps, pe));
        Ok(v)
    }

    /// Verify the peer's confirm body.
    pub fn verify_confirm(&self, body: &[u8]) -> Result<(), SaeErr> {
        if body.len() < 34 { return Err(SaeErr::Malformed); }
        let (ps, pe) = self.peer.as_ref().ok_or(SaeErr::NoSecret)?;
        let sc = u16::from_le_bytes([body[0], body[1]]);
        let want = self.confirm_for(sc, ps, pe, &self.scalar, &self.element);
        let mut d = 0u8;
        for i in 0..32 { d |= want[i] ^ body[2 + i]; }
        if d == 0 { Ok(()) } else { Err(SaeErr::BadConfirm) }
    }
}

// ── frames ──────────────────────────────────────────────────────────────────

/// Authentication frame body (after the MAC header) for an SAE commit.
/// `status` 0 (hunt-and-peck) or 126 (H2E); `token` the anti-clogging token
/// the AP asked for, if any.
pub fn commit_frame(status: u16, token: &[u8], commit: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&AUTH_ALG_SAE.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&status.to_le_bytes());
    v.extend_from_slice(&GROUP.to_le_bytes());
    v.extend_from_slice(token);
    v.extend_from_slice(commit);
    v
}

pub fn confirm_frame(confirm: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&AUTH_ALG_SAE.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(confirm);
    v
}

/// The anti-clogging-token request: status 76, group id, token.
pub fn anti_clogging_frame(token: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&AUTH_ALG_SAE.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&STATUS_ANTI_CLOGGING.to_le_bytes());
    v.extend_from_slice(&GROUP.to_le_bytes());
    v.extend_from_slice(token);
    v
}

/// A parsed SAE authentication frame body.
pub struct AuthBody<'a> { pub seq: u16, pub status: u16, pub rest: &'a [u8] }

pub fn parse_auth(b: &[u8]) -> Option<AuthBody<'_>> {
    if b.len() < 6 || u16::from_le_bytes([b[0], b[1]]) != AUTH_ALG_SAE { return None; }
    Some(AuthBody { seq: u16::from_le_bytes([b[2], b[3]]), status: u16::from_le_bytes([b[4], b[5]]), rest: &b[6..] })
}

/// Anti-clogging token for `sta` under the AP's rotating `secret`.
pub fn make_token(secret: &[u8; 32], sta: &[u8; 6]) -> [u8; 32] { wc::hmac_sha256(secret, &[sta]) }

// ── known-answer tests ──────────────────────────────────────────────────────

fn unhex32(s: &str) -> [u8; 32] {
    let mut o = [0u8; 32];
    let b = s.as_bytes();
    let v = |c: u8| -> u8 { match c { b'0'..=b'9' => c - b'0', b'a'..=b'f' => c - b'a' + 10, _ => c - b'A' + 10 } };
    for i in 0..32 { o[i] = v(b[2 * i]) << 4 | v(b[2 * i + 1]); }
    o
}

/// Known-answer and protocol self-checks; `Err` names the failing check.
pub fn selftest(rng: &mut dyn FnMut(&mut [u8])) -> Result<(), &'static str> {
    // NIST: 1*G, 2*G, RFC 5903 section 8.1 (group 19) public key, and n*G = infinity.
    let mut one = [0u8; 32];
    one[31] = 1;
    let g = base_mul(&one).ok_or("1G")?;
    if g.x != to_be(&GX) || g.y != to_be(&GY) { return Err("1G value"); }
    let mut two = [0u8; 32];
    two[31] = 2;
    let g2 = base_mul(&two).ok_or("2G")?;
    if g2.x != unhex32("7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc47669978")
        || g2.y != unhex32("07775510db8ed040293d9ac69f7430dbba7dade63ce982299e04b79d227873d1") { return Err("2G value"); }
    let gi = base_mul(&unhex32("c88f01f510d9ac3f70a292daa2316de544e9aab8afe84049c62a9c57862d1433")).ok_or("gi")?;
    if gi.x != unhex32("dad0b65394221cf9b051e1feca5787d098dfe637fc90b9ef945d0c3772581180")
        || gi.y != unhex32("5271a0461cdb8252d61f1c456fa3e59ab1f45b33accf5f58389e0577b8990bb3") { return Err("RFC 5903 public key"); }
    if base_mul(&to_be(&N)).is_some() { return Err("nG not infinity"); }
    // RFC 9380 J.1.1 (P256_XMD:SHA-256_SSWU_RO_, msg ""): map_to_curve of u[0] and u[1].
    let q0 = map_to_curve(&unhex32("ad5342c66a6dd0ff080df1da0ea1c04b96e0330dd89406465eeba11582515009"));
    if q0.x != unhex32("ab640a12220d3ff283510ff3f4b1953d09fad35795140b1c5d64f313967934d5")
        || q0.y != unhex32("dccb558863804a881d4fff3455716c836cef230e5209594ddd33d85c565b19b1") { return Err("RFC 9380 SSWU Q0"); }
    let q1 = map_to_curve(&unhex32("8c0f1d43204bd6f6ea70ae8013070a1518b43873bcd850aafa0a9e220e2eea5a"));
    if q1.x != unhex32("51cce63c50d972a6e51c61334f0f4875c9ac1cd2d3238412f84e31da7d980ef5")
        || q1.y != unhex32("b45d1a36d00ad90e5ec7840a60a4de411917fbe7c82c3949a6e699e5a1b66aac") { return Err("RFC 9380 SSWU Q1"); }
    // Both roles agree, hunt-and-peck and H2E; a wrong password does not.
    let (a, b) = ([0x4d, 0x3f, 0x2f, 0xff, 0xe3, 0x87], [0xa5, 0xd8, 0xaa, 0x95, 0x8e, 0x3c]);
    for h2e in [false, true] {
        let mk = |pw: &[u8], x: &[u8; 6], y: &[u8; 6]| -> Pt {
            if h2e { pwe_from_pt(&pt_h2e(b"TestSsid", pw, None), x, y) } else { pwe_hunt_and_peck(pw, x, y, None) }
        };
        if mk(b"mekmitasdigoat", &a, &b) != mk(b"mekmitasdigoat", &b, &a) { return Err("PWE not symmetric"); }
        let mut s1 = Sae::new(mk(b"mekmitasdigoat", &a, &b), rng);
        let mut s2 = Sae::new(mk(b"mekmitasdigoat", &b, &a), rng);
        s1.process_commit(&s2.commit_body()).map_err(|_| "commit 1")?;
        s2.process_commit(&s1.commit_body()).map_err(|_| "commit 2")?;
        if s1.pmk != s2.pmk || s1.kck != s2.kck || s1.pmkid != s2.pmkid { return Err("keys differ"); }
        s2.verify_confirm(&s1.confirm_body().map_err(|_| "cb")?).map_err(|_| "confirm 1->2")?;
        s1.verify_confirm(&s2.confirm_body().map_err(|_| "cb")?).map_err(|_| "confirm 2->1")?;
        let mut s3 = Sae::new(mk(b"wrong-password", &b, &a), rng);
        s3.process_commit(&s1.commit_body()).map_err(|_| "commit 3")?;
        if s3.pmk == s1.pmk { return Err("wrong password agreed"); }
        if s1.verify_confirm(&s3.confirm_body().map_err(|_| "cb")?).is_ok() { return Err("wrong password confirmed"); }
        if s1.process_commit(&s1.commit_body()) != Err(SaeErr::Reflection) { return Err("reflection accepted"); }
    }
    Ok(())
}
