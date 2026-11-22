//! `RevokedKeys`: a plain public-key list or an OpenSSH binary KRL (PROTOCOL.krl).
//!
//! As OpenSSH (`sshkey_check_revoked`): the file is tried as a KRL first, else as a
//! list of public keys. A user certificate is revoked when its subject key, its CA
//! key, the certificate itself, its serial (under its CA) or its key id is listed.
//! An unreadable or malformed file revokes everything (`Err`): the caller refuses
//! public-key authentication. KRL signatures are not verified (sshd does not either);
//! SHA1 fingerprint sections are not supported (sections are skipped).

use russh::keys::ssh_key::public::KeyData;
use russh::keys::{Certificate, PublicKey};
use sha2::{Digest, Sha256};

const MAGIC: &[u8] = b"SSHKRL\n\0";

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Default)]
struct CaRevocations {
    /// The CA key blob; empty = applies to every CA.
    ca: Vec<u8>,
    serials: Vec<(u64, u64)>,
    ids: Vec<Vec<u8>>,
}

#[derive(Default)]
struct Krl {
    blobs: Vec<Vec<u8>>,
    sha256: Vec<Vec<u8>>,
    certs: Vec<CaRevocations>,
}

fn blob(k: &KeyData) -> Vec<u8> {
    PublicKey::from(k.clone()).to_bytes().unwrap_or_default()
}

fn parse_certs(body: &[u8]) -> Option<CaRevocations> {
    let mut r = Rd(body);
    let mut out = CaRevocations { ca: r.string()?.to_vec(), ..Default::default() };
    r.string()?; // reserved
    while !r.empty() {
        let t = r.u8()?;
        let mut d = Rd(r.string()?);
        match t {
            0x20 => {
                while !d.empty() {
                    let s = d.u64()?;
                    out.serials.push((s, s));
                }
            }
            0x21 => out.serials.push((d.u64()?, d.u64()?)),
            0x22 => {
                let off = d.u64()?;
                let m = d.string()?;
                for i in 0..m.len() * 8 {
                    if m[m.len() - 1 - i / 8] >> (i % 8) & 1 == 1 {
                        let s = off.checked_add(i as u64)?;
                        out.serials.push((s, s));
                    }
                }
            }
            0x23 => {
                while !d.empty() {
                    out.ids.push(d.string()?.to_vec());
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn parse_krl(data: &[u8]) -> Option<Krl> {
    let mut r = Rd(data.strip_prefix(MAGIC)?);
    if r.u32()? != 1 {
        return None;
    }
    r.u64()?; // krl_version
    r.u64()?; // generated_date
    r.u64()?; // flags
    r.string()?; // reserved
    r.string()?; // comment
    let mut k = Krl::default();
    while !r.empty() {
        let t = r.u8()?;
        let body = r.string()?;
        let mut b = Rd(body);
        match t {
            1 => k.certs.push(parse_certs(body)?),
            2 => {
                while !b.empty() {
                    k.blobs.push(b.string()?.to_vec());
                }
            }
            3 => {} // SHA1 fingerprints: unsupported, skipped
            4 => {} // signature: not verified
            5 => {
                while !b.empty() {
                    k.sha256.push(b.string()?.to_vec());
                }
            }
            _ => return None,
        }
    }
    Some(k)
}

impl Krl {
    fn has_key(&self, k: &KeyData) -> bool {
        let b = blob(k);
        self.blobs.contains(&b) || self.sha256.iter().any(|h| h[..] == Sha256::digest(&b)[..])
    }

    fn revoked(&self, subject: &KeyData, cert: Option<&Certificate>) -> bool {
        if self.has_key(subject) {
            return true;
        }
        let Some(c) = cert else { return false };
        if self.has_key(c.signature_key()) {
            return true;
        }
        let ca = blob(c.signature_key());
        self.certs.iter().filter(|r| r.ca.is_empty() || r.ca == ca).any(|r| {
            (c.serial() != 0 && r.serials.iter().any(|&(lo, hi)| (lo..=hi).contains(&c.serial())))
                || r.ids.iter().any(|i| i[..] == *c.key_id().as_bytes())
        })
    }
}

/// Is the key (a certificate's subject key, with the certificate) revoked by `file`?
/// `Err` = the file cannot be used: refuse the authentication.
pub fn is_revoked(file: &str, subject: &KeyData, cert: Option<&Certificate>) -> Result<bool, String> {
    let data = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
    if data.starts_with(MAGIC) {
        return Ok(parse_krl(&data).ok_or_else(|| format!("{file}: bad KRL"))?.revoked(subject, cert));
    }
    let text = String::from_utf8_lossy(&data);
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if let Ok(k) = PublicKey::from_openssh(l) {
            if k.key_data() == subject || cert.map_or(false, |c| k.key_data() == c.signature_key()) {
                return Ok(true);
            }
        } else if let (Ok(lc), Some(c)) = (Certificate::from_openssh(l), cert) {
            if lc.public_key() == c.public_key()
                && lc.signature_key() == c.signature_key()
                && lc.serial() == c.serial()
                && lc.key_id() == c.key_id()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
