//! Publickey userauth re-verification and the host-key signer, both in-process
//! for now (privsep phase P1). The checks here are exactly what a separate
//! monitor process will perform on the raw signature (phase P3): the signed
//! blob must be `string(session_id) || USERAUTH_REQUEST(user, "ssh-connection",
//! "publickey", TRUE, algo, keyblob)` with the session id the signer recorded,
//! and the signature must verify against the announced key.

use std::sync::{Arc, Mutex, OnceLock};

use russh::keys::ssh_key::{Algorithm, HashAlg, PrivateKey, PublicKey, Signature};
use russh::PublicKeyOrCertificate;
use signature::Verifier;
use ssh_encoding::Decode;

const MSG_USERAUTH_REQUEST: u8 = 50;

/// The session id (first exchange hash signed) of this connection. One
/// connection per `sshd -R` process.
static SESSION_ID: OnceLock<Vec<u8>> = OnceLock::new();

pub fn recorded_session_id() -> Option<&'static [u8]> {
    SESSION_ID.get().map(|v| v.as_slice())
}

/// The kex hook: signs the exchange hash with the real host key and records
/// the first hash as the session id.
pub fn host_key_signer(keys: Vec<PrivateKey>) -> russh::server::HostKeySigner {
    let keys = Arc::new(Mutex::new(keys));
    Arc::new(move |alg: &Algorithm, hash: Option<HashAlg>, h: &[u8]| {
        if !matches!(h.len(), 32 | 48 | 64) {
            return Err(russh::keys::ssh_key::Error::Encoding(ssh_encoding::Error::Length));
        }
        let _ = SESSION_ID.set(h.to_vec());
        let keys = keys.lock().unwrap();
        let key = keys
            .iter()
            .find(|k| k.algorithm() == *alg || (matches!(alg, Algorithm::Rsa { .. }) && matches!(k.algorithm(), Algorithm::Rsa { .. })))
            .ok_or(russh::keys::ssh_key::Error::AlgorithmUnknown)?;
        russh::sign_with_hash_alg(&russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key.clone()), hash), h)
    })
}

struct Rd<'a>(&'a [u8]);
impl<'a> Rd<'a> {
    fn u8(&mut self) -> Option<u8> {
        let (b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(*b)
    }
    fn string(&mut self) -> Option<&'a [u8]> {
        let n = u32::from_be_bytes(self.0.get(..4)?.try_into().ok()?) as usize;
        let s = self.0.get(4..4usize.checked_add(n)?)?;
        self.0 = &self.0[4 + n..];
        Some(s)
    }
    fn done(&self) -> bool {
        self.0.is_empty()
    }
}

/// Verify a raw publickey userauth signature. `session_id` is what russh
/// signed over; it must equal the one the signer recorded.
pub fn verify_userauth(
    user: &str,
    key: &PublicKeyOrCertificate,
    signed_data: &[u8],
    sig_blob: &[u8],
    session_id: &[u8],
    recorded: Option<&[u8]>,
) -> bool {
    verify_inner(user, key, signed_data, sig_blob, session_id, recorded).is_some()
}

fn verify_inner(
    user: &str,
    key: &PublicKeyOrCertificate,
    signed_data: &[u8],
    sig_blob: &[u8],
    session_id: &[u8],
    recorded: Option<&[u8]>,
) -> Option<()> {
    if recorded? != session_id {
        return None;
    }
    let mut r = Rd(signed_data);
    if r.string()? != session_id || r.u8()? != MSG_USERAUTH_REQUEST {
        return None;
    }
    if r.string()? != user.as_bytes() || r.string()? != b"ssh-connection" || r.string()? != b"publickey" || r.u8()? != 1 {
        return None;
    }
    let algo = std::str::from_utf8(r.string()?).ok()?;
    let blob = r.string()?;
    if !r.done() {
        return None;
    }
    // The announced blob must be the very key the handler is deciding about.
    let (pubkey, want_blob, requested) = match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => (key.clone(), key.to_bytes().ok()?, Algorithm::new(algo).ok()?),
        PublicKeyOrCertificate::Certificate(c) => (
            PublicKey::new(c.public_key().clone(), ""),
            c.to_bytes().ok()?,
            Algorithm::new(algo.strip_suffix("-cert-v01@openssh.com")?).ok()?,
        ),
    };
    if blob != want_blob.as_slice() {
        return None;
    }
    let mut rdr = sig_blob;
    let sig = Signature::decode(&mut rdr).ok()?;
    if !rdr.is_empty() || sig.algorithm() != requested {
        return None;
    }
    Verifier::verify(&pubkey, signed_data, &sig).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_encoding::Encode;
    use russh::keys::ssh_key::private::Ed25519Keypair;

    fn kp(seed: u8) -> (PrivateKey, PublicKey) {
        let k = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
        let p = k.public_key().clone();
        (k, p)
    }

    fn string(out: &mut Vec<u8>, s: &[u8]) {
        out.extend((s.len() as u32).to_be_bytes());
        out.extend(s);
    }

    fn blob(sid: &[u8], user: &str, key: &PublicKey) -> Vec<u8> {
        let mut b = Vec::new();
        string(&mut b, sid);
        b.push(MSG_USERAUTH_REQUEST);
        string(&mut b, user.as_bytes());
        string(&mut b, b"ssh-connection");
        string(&mut b, b"publickey");
        b.push(1);
        string(&mut b, b"ssh-ed25519");
        string(&mut b, &key.to_bytes().unwrap());
        b
    }

    fn sign(k: &PrivateKey, data: &[u8]) -> Vec<u8> {
        let s: Signature = signature::Signer::try_sign(k.key_data(), data).unwrap();
        let mut v = Vec::new();
        s.encode(&mut v).unwrap();
        v
    }

    fn pk(p: &PublicKey) -> PublicKeyOrCertificate {
        PublicKeyOrCertificate::PublicKey { key: p.clone(), hash_alg: None }
    }

    const SID: [u8; 32] = [9; 32];

    #[test]
    fn valid_accepted() {
        let (k, p) = kp(1);
        let d = blob(&SID, "root", &p);
        assert!(verify_userauth("root", &pk(&p), &d, &sign(&k, &d), &SID, Some(&SID)));
    }

    #[test]
    fn forged_signature_on_valid_key_rejected() {
        let (_, p) = kp(1);
        let (other, _) = kp(2);
        let d = blob(&SID, "root", &p);
        assert!(!verify_userauth("root", &pk(&p), &d, &sign(&other, &d), &SID, Some(&SID)));
    }

    #[test]
    fn wrong_session_id_rejected() {
        let (k, p) = kp(1);
        let evil = [7u8; 32];
        let d = blob(&evil, "root", &p);
        // signature valid for the evil id, but the signer recorded SID
        assert!(!verify_userauth("root", &pk(&p), &d, &sign(&k, &d), &evil, Some(&SID)));
        // blob carries another id than the one reported
        let d = blob(&evil, "root", &p);
        assert!(!verify_userauth("root", &pk(&p), &d, &sign(&k, &d), &SID, Some(&SID)));
        // nothing recorded yet
        let d = blob(&SID, "root", &p);
        assert!(!verify_userauth("root", &pk(&p), &d, &sign(&k, &d), &SID, None));
    }

    #[test]
    fn wrong_user_rejected() {
        let (k, p) = kp(1);
        let d = blob(&SID, "root", &p);
        assert!(!verify_userauth("leandro", &pk(&p), &d, &sign(&k, &d), &SID, Some(&SID)));
    }

    #[test]
    fn trailing_bytes_rejected() {
        let (k, p) = kp(1);
        let mut d = blob(&SID, "root", &p);
        d.push(0);
        assert!(!verify_userauth("root", &pk(&p), &d, &sign(&k, &d), &SID, Some(&SID)));
        let d = blob(&SID, "root", &p);
        let mut s = sign(&k, &d);
        s.push(0);
        assert!(!verify_userauth("root", &pk(&p), &d, &s, &SID, Some(&SID)));
    }

    #[test]
    fn replay_for_another_key_rejected() {
        let (k1, p1) = kp(1);
        let (_, p2) = kp(2);
        let d = blob(&SID, "root", &p1);
        let s = sign(&k1, &d);
        // valid blob+signature of key 1, presented as key 2's authentication
        assert!(!verify_userauth("root", &pk(&p2), &d, &s, &SID, Some(&SID)));
    }

    #[test]
    fn algorithm_mismatch_rejected() {
        let (k, p) = kp(1);
        let mut d = Vec::new();
        string(&mut d, &SID);
        d.push(MSG_USERAUTH_REQUEST);
        string(&mut d, b"root");
        string(&mut d, b"ssh-connection");
        string(&mut d, b"publickey");
        d.push(1);
        string(&mut d, b"ssh-rsa");
        string(&mut d, &p.to_bytes().unwrap());
        assert!(!verify_userauth("root", &pk(&p), &d, &sign(&k, &d), &SID, Some(&SID)));
    }

    #[test]
    fn signer_rejects_odd_hash_len() {
        let (k, _) = kp(1);
        let s = host_key_signer(vec![k]);
        assert!(s(&Algorithm::Ed25519, None, &[0u8; 5]).is_err());
    }
}
