//! SHA-256 / HMAC-SHA256 / IEEE 802.11 KDF-SHA256 / AES-128-CMAC vectors and a
//! full AKM 6 (PSK-SHA256, key descriptor version 3) handshake with PMF (IGTK).

use crate::wifi_crypto::*;
use alloc::vec;
use alloc::vec::Vec;

fn unhex(s: &str) -> Vec<u8> {
    let s: alloc::string::String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

#[test]
fn sha256_fips180_vectors() {
    assert_eq!(sha256(b"abc").to_vec(), unhex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
    let m = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    assert_eq!(sha256(m).to_vec(), unhex("248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"));
    // Streaming across block boundaries matches one-shot.
    let mut s = Sha256::new();
    for c in m.chunks(7) { s.update(c); }
    assert_eq!(s.finish(), sha256(m));
}

#[test]
fn hmac_sha256_rfc4231() {
    // Test case 1.
    assert_eq!(hmac_sha256(&[0x0b; 20], &[b"Hi There"]).to_vec(),
               unhex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"));
    // Test case 6 (key longer than the block).
    assert_eq!(hmac_sha256(&[0xaa; 131], &[b"Test Using Larger Than ", b"Block-Size Key - Hash Key First"]).to_vec(),
               unhex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"));
}

#[test]
fn aes_cmac_rfc4493() {
    let k: [u8; 16] = unhex("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
    let m = unhex("6bc1bee22e409f96e93d7e117393172a ae2d8a571e03ac9c9eb76fac45af8e51
                   30c81c46a35ce411e5fbc1191a0a52ef f69f2445df4f9b17ad2b417be66c3710");
    for (n, want) in [(0, "bb1d6929e95937287fa37d129b756746"), (16, "070a16b46b4d4144f79bdd9dd04a287c"),
                      (40, "dfa66747de9ae63030ca32611497c827"), (64, "51f0bebf7e3b9d92fc49741779363cfe")] {
        assert_eq!(aes_cmac(&k, &[&m[..n]]).to_vec(), unhex(want), "len {}", n);
        // Split input gives the same MAC.
        let h = n / 3;
        assert_eq!(aes_cmac(&k, &[&m[..h], &m[h..n]]).to_vec(), unhex(want), "split len {}", n);
    }
}

const AP: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const STA: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];

fn nonces() -> ([u8; 32], [u8; 32]) {
    let mut an = [0u8; 32];
    let mut sn = [0u8; 32];
    for i in 0..32 { an[i] = 0xa0 + (i % 16) as u8; sn[i] = 0x50 + (i % 16) as u8; }
    (an, sn)
}

/// KDF-SHA256-384 PTK (802.11-2016 12.7.1.6.2 / 12.7.1.3) against an
/// independent reference (Python hmac/hashlib computing the same definition).
#[test]
fn ptk_kdf_sha256_reference() {
    let mut pmk = [0u8; 32];
    for (i, b) in pmk.iter_mut().enumerate() { *b = i as u8; }
    let (an, sn) = nonces();
    let ptk = derive_ptk_sha256(&pmk, &AP, &STA, &an, &sn);
    let want = unhex("120c89442fc343218a21e7f23ea4ad49dabbc55b116aae7c5463a329745aaf07
                      7c11f6e68981019a225957c1d2e1bff7");
    let mut got = ptk.kck.to_vec();
    got.extend_from_slice(&ptk.kek);
    got.extend_from_slice(&ptk.tk);
    assert_eq!(got, want);
    // Symmetric in the address/nonce order, and different from the SHA-1 PRF.
    let p2 = derive_ptk_sha256(&pmk, &STA, &AP, &sn, &an);
    assert_eq!(p2.tk, ptk.tk);
    assert_ne!(derive_ptk(&pmk, &AP, &STA, &an, &sn).kck, ptk.kck);
}

#[test]
fn rsn_ie_akm_and_caps() {
    let ie = rsn_ie_psk_akm(0x000f_ac04, 6, RSN_CAP_MFPC);
    assert_eq!(ie.to_vec(), unhex("30140100000fac040100000fac040100000fac068000"));
    assert_eq!(rsn_ie_psk_akm(0x000f_ac04, 2, 0), rsn_ie_psk_ccmp());
}

fn igtk_kde(key_id: u16, ipn: [u8; 6], igtk: &[u8; 16]) -> Vec<u8> {
    let mut v = vec![0xdd, 28, 0x00, 0x0f, 0xac, 0x09];
    v.extend_from_slice(&key_id.to_le_bytes());
    v.extend_from_slice(&ipn);
    v.extend_from_slice(igtk);
    v
}

fn pad(mut d: Vec<u8>) -> Vec<u8> {
    d.push(0xdd);
    while d.len() % 8 != 0 || d.len() < 16 { d.push(0); }
    d
}

/// Full v3 handshake: the authenticator side is written here from the
/// standard (KDF-SHA256 PTK, AES-CMAC MIC, AES key wrap) and checks the
/// station's message 2 MIC and RSN IE; the station installs PTK, GTK, IGTK.
#[test]
fn akm6_handshake_with_igtk() {
    let pmk = wpa_psk(b"correct horse", b"LeandrOS-Net");
    let (an, sn) = nonces();
    let ie = rsn_ie_psk_akm(0x000f_ac04, 6, RSN_CAP_MFPC).to_vec();
    let mut sup = Supplicant::new(pmk, STA, AP, ie.clone(), sn);
    sup.set_akm(6);
    let ki = |f: u16| 3 | f;
    let msg1 = build_eapol_key(2, ki(key_info::TYPE_PAIRWISE | key_info::ACK), 16, &1u64.to_be_bytes(), &an, &[]);
    let mut ev = sup.on_eapol(&msg1);
    let HsEvent::Send(msg2) = ev.remove(0) else { panic!("msg2") };
    assert_eq!(sup.descriptor_version(), 3);
    let k = parse_eapol_key(&msg2).unwrap();
    assert_eq!(k.key_info, ki(key_info::TYPE_PAIRWISE | key_info::MIC));
    assert_eq!(k.key_data, &ie[..], "msg2 IE must be the association IE byte for byte");
    let ptk = derive_ptk_sha256(&pmk, &AP, &STA, &an, &k.nonce);
    // Independent CMAC check of message 2.
    let mut z = msg2.clone();
    z[81..97].fill(0);
    assert_eq!(aes_cmac(&ptk.kck, &[&z]).to_vec(), msg2[81..97].to_vec());
    assert!(eapol_mic_ok(&ptk.kck, &msg2));
    // A SHA-1 (v2) MIC over the same bytes would not match.
    assert_ne!(hmac_sha1(&ptk.kck, &[&z])[..16], msg2[81..97]);

    let gtk = [0x77u8; 16];
    let igtk = [0x42u8; 16];
    let ipn = [9, 0, 0, 0, 0, 0];
    let mut kd = ie.clone();
    kd.extend_from_slice(&[0xdd, 22, 0x00, 0x0f, 0xac, 0x01, 0x01, 0x00]);
    kd.extend_from_slice(&gtk);
    kd.extend_from_slice(&igtk_kde(4, ipn, &igtk));
    let wrapped = aes_key_wrap(&ptk.kek, &pad(kd));
    let mut msg3 = build_eapol_key(2, ki(key_info::TYPE_PAIRWISE | key_info::INSTALL | key_info::ACK | key_info::MIC
                                         | key_info::SECURE | key_info::ENCRYPTED_KEY_DATA),
                                   16, &2u64.to_be_bytes(), &an, &wrapped);
    eapol_set_mic(&ptk.kck, &mut msg3);
    let mut ev = sup.on_eapol(&msg3);
    let HsEvent::Send(msg4) = ev.remove(0) else { panic!("msg4") };
    assert!(eapol_mic_ok(&ptk.kck, &msg4));
    assert_eq!(parse_eapol_key(&msg4).unwrap().key_info, ki(key_info::TYPE_PAIRWISE | key_info::MIC | key_info::SECURE));
    assert!(matches!(ev.remove(0), HsEvent::InstallPtk(tk) if tk == ptk.tk));
    assert!(matches!(ev.remove(0), HsEvent::InstallGtk { key_id: 1, .. }));
    match ev.remove(0) {
        HsEvent::InstallIgtk { key_id, key, ipn: p } => { assert_eq!(key_id, 4); assert_eq!(key, igtk); assert_eq!(p, ipn); }
        _ => panic!("expected InstallIgtk"),
    }
    assert!(matches!(ev.remove(0), HsEvent::Complete));
}

/// A v2 message 1 with AKM 6 configured still derives the PTK with
/// KDF-SHA256 (AKM decides the KDF); a v1 descriptor is refused.
#[test]
fn akm_decides_kdf_and_v1_refused() {
    let pmk = [7u8; 32];
    let (an, sn) = nonces();
    let mut sup = Supplicant::new(pmk, STA, AP, rsn_ie_psk_ccmp().to_vec(), sn);
    let m1 = build_eapol_key(2, 1 | key_info::TYPE_PAIRWISE | key_info::ACK, 16, &1u64.to_be_bytes(), &an, &[]);
    assert!(matches!(sup.on_eapol(&m1).remove(0), HsEvent::Error(_)));
    let mut sup = Supplicant::new(pmk, STA, AP, rsn_ie_psk_ccmp().to_vec(), sn);
    sup.set_akm(6);
    let m1 = build_eapol_key(2, 2 | key_info::TYPE_PAIRWISE | key_info::ACK, 16, &1u64.to_be_bytes(), &an, &[]);
    let HsEvent::Send(m2) = sup.on_eapol(&m1).remove(0) else { panic!() };
    let ptk = derive_ptk_sha256(&pmk, &AP, &STA, &an, &sn);
    assert!(eapol_mic_ok(&ptk.kck, &m2));
}
