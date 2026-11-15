//! Compiles sshd's `userauth.rs` on the host so its unit tests run without QEMU.
#[path = "../../src/userauth.rs"]
pub mod userauth;

#[cfg(test)]
mod extra {
    use crate::userauth::*;
    use russh::keys::ssh_key::certificate::{Builder, CertType};
    use russh::keys::ssh_key::private::Ed25519Keypair;
    use russh::keys::ssh_key::{Algorithm, HashAlg, PrivateKey, Signature};
    use russh::keys::PrivateKeyWithHashAlg;
    use russh::PublicKeyOrCertificate;
    use ssh_encoding::Decode;
    use std::sync::Arc;

    const SID: [u8; 32] = [9; 32];
    const RSA_PEM: &str = include_str!("rsa_test_key.pem");

    fn string(out: &mut Vec<u8>, s: &[u8]) {
        out.extend((s.len() as u32).to_be_bytes());
        out.extend(s);
    }

    /// signed blob announcing `algo` with key/cert blob `kb`
    fn blob(user: &str, algo: &str, kb: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        string(&mut b, &SID);
        b.push(50);
        string(&mut b, user.as_bytes());
        string(&mut b, b"ssh-connection");
        string(&mut b, b"publickey");
        b.push(1);
        string(&mut b, algo.as_bytes());
        string(&mut b, kb);
        b
    }

    fn sign(k: &PrivateKey, hash: Option<HashAlg>, data: &[u8]) -> Vec<u8> {
        russh::sign_with_hash_alg(&PrivateKeyWithHashAlg::new(Arc::new(k.clone()), hash), data).unwrap()
    }

    fn rsa() -> PrivateKey {
        PrivateKey::from_openssh(RSA_PEM).unwrap()
    }

    fn ed(seed: u8) -> PrivateKey {
        PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
    }

    fn plain(k: &PrivateKey) -> PublicKeyOrCertificate {
        PublicKeyOrCertificate::PublicKey { key: k.public_key().clone(), hash_alg: None }
    }

    fn ok(user: &str, key: &PublicKeyOrCertificate, d: &[u8], s: &[u8]) -> bool {
        verify_userauth(user, key, d, s, &SID, Some(&SID))
    }

    fn cert_for(ca: &PrivateKey, subject: &PrivateKey) -> russh::keys::ssh_key::Certificate {
        let mut b = Builder::new(vec![1u8; 32], subject.public_key().key_data().clone(), 0, u64::MAX).unwrap();
        b.cert_type(CertType::User).unwrap();
        b.key_id("t").unwrap();
        b.all_principals_valid().unwrap();
        b.sign(ca).unwrap()
    }

    #[test]
    fn rsa_sha2_accepted_sha1_rejected() {
        let k = rsa();
        let kb = k.public_key().to_bytes().unwrap();
        for (name, h, want) in [
            ("rsa-sha2-256", Some(HashAlg::Sha256), true),
            ("rsa-sha2-512", Some(HashAlg::Sha512), true),
            ("ssh-rsa", None, false),
        ] {
            let d = blob("root", name, &kb);
            assert_eq!(ok("root", &plain(&k), &d, &sign(&k, h, &d)), want, "{name}");
        }
    }

    #[test]
    fn rsa_announced_sha2_signed_sha1_rejected() {
        let k = rsa();
        let kb = k.public_key().to_bytes().unwrap();
        let d = blob("root", "rsa-sha2-512", &kb);
        assert!(!ok("root", &plain(&k), &d, &sign(&k, None, &d)));
        assert!(!ok("root", &plain(&k), &d, &sign(&k, Some(HashAlg::Sha256), &d)));
    }

    #[test]
    fn ed25519_cert_accepted_plain_vs_cert_confusion_rejected() {
        let (ca, user) = (ed(5), ed(6));
        let c = cert_for(&ca, &user);
        let cb = c.to_bytes().unwrap();
        let ck = PublicKeyOrCertificate::Certificate(c);
        let d = blob("root", "ssh-ed25519-cert-v01@openssh.com", &cb);
        assert!(ok("root", &ck, &d, &sign(&user, None, &d)));
        // cert blob announced under the plain algorithm name
        let d = blob("root", "ssh-ed25519", &cb);
        assert!(!ok("root", &ck, &d, &sign(&user, None, &d)));
        // plain key under a cert algorithm name
        let kb = user.public_key().to_bytes().unwrap();
        let d = blob("root", "ssh-ed25519-cert-v01@openssh.com", &kb);
        assert!(!ok("root", &plain(&user), &d, &sign(&user, None, &d)));
    }

    #[test]
    fn rsa_cert_sha2_accepted_sha1_rejected() {
        let (ca, user) = (ed(5), rsa());
        let c = cert_for(&ca, &user);
        let cb = c.to_bytes().unwrap();
        let ck = PublicKeyOrCertificate::Certificate(c);
        let d = blob("root", "rsa-sha2-512-cert-v01@openssh.com", &cb);
        assert!(ok("root", &ck, &d, &sign(&user, Some(HashAlg::Sha512), &d)));
        let d = blob("root", "ssh-rsa-cert-v01@openssh.com", &cb);
        assert!(!ok("root", &ck, &d, &sign(&user, None, &d)));
    }

    #[test]
    fn sk_cert_algorithm_names_map() {
        assert_eq!(cert_algorithm("sk-ssh-ed25519-cert-v01@openssh.com"), Some(Algorithm::SkEd25519));
        assert_eq!(cert_algorithm("sk-ecdsa-sha2-nistp256-cert-v01@openssh.com"), Some(Algorithm::SkEcdsaSha2NistP256));
        assert_eq!(cert_algorithm("rsa-sha2-256-cert-v01@openssh.com"), Some(Algorithm::Rsa { hash: Some(HashAlg::Sha256) }));
        assert_eq!(cert_algorithm("sk-ssh-ed25519"), None);
        assert_eq!(cert_algorithm("ssh-ed25519"), None);
    }

    #[test]
    fn flag_not_one_and_truncated_rejected() {
        let k = ed(1);
        let kb = k.public_key().to_bytes().unwrap();
        let mut d = blob("root", "ssh-ed25519", &kb);
        let s = sign(&k, None, &d);
        assert!(ok("root", &plain(&k), &d, &s));
        let flag_at = 4 + 32 + 1 + 4 + 4 + 4 + 14 + 4 + 9;
        assert_eq!(d[flag_at], 1);
        d[flag_at] = 2;
        assert!(!ok("root", &plain(&k), &d, &sign(&k, None, &d)));
        let d = blob("root", "ssh-ed25519", &kb);
        for n in [0, 1, 5, d.len() / 2, d.len() - 1] {
            assert!(!ok("root", &plain(&k), &d[..n], &s), "len {n}");
        }
        assert!(!ok("root", &plain(&k), &d, &[]));
        assert!(Signature::decode(&mut &s[..s.len() - 1]).is_err());
    }

    /// The first hash signed is the session id; a later (rekey) hash must not
    /// replace it. Only test touching the process-global with a valid length.
    #[test]
    fn rekey_hash_does_not_replace_session_id() {
        let s = host_key_signer(vec![ed(1)]);
        s(&Algorithm::Ed25519, None, &[1u8; 32]).unwrap();
        s(&Algorithm::Ed25519, None, &[2u8; 32]).unwrap();
        assert_eq!(recorded_session_id(), Some(&[1u8; 32][..]));
    }
}
