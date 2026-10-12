//! Standard test vectors for the hand-rolled WPA2 primitives, plus a full
//! 4-way + group key handshake against a minimal in-test authenticator.

use crate::wifi_crypto::*;
use alloc::vec;
use alloc::vec::Vec;

fn unhex(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    assert!(b.len() % 2 == 0);
    (0..b.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

#[test]
fn sha1_vectors() {
    assert_eq!(
        sha1(b"abc").to_vec(),
        unhex("a9993e364706816aba3e25717850c26c9cd0d89d")
    );
    assert_eq!(
        sha1(b"").to_vec(),
        unhex("da39a3ee5e6b4b0d3255bfef95601890afd80709")
    );
    assert_eq!(
        sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq").to_vec(),
        unhex("84983e441c3bd26ebaae4aa1f95129e5e54670f1")
    );
    let mut s = Sha1::new();
    let chunk = [b'a'; 1000];
    for _ in 0..1000 {
        s.update(&chunk);
    }
    assert_eq!(
        s.finish().to_vec(),
        unhex("34aa973cd4c4daa4f61eeb2bdbad27316534016f")
    );
}

#[test]
fn sha1_streaming_matches_one_shot() {
    let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
    for step in [1usize, 3, 7, 13, 63, 64, 65, 129] {
        let mut s = Sha1::new();
        for c in data.chunks(step) {
            s.update(c);
        }
        assert_eq!(s.finish(), sha1(&data), "step {}", step);
    }
    // Lengths around the padding boundaries.
    for n in 50..70 {
        let mut s = Sha1::new();
        s.update(&data[..n / 2]);
        s.update(&data[n / 2..n]);
        assert_eq!(s.finish(), sha1(&data[..n]));
    }
}

#[test]
fn hmac_sha1_rfc2202() {
    assert_eq!(
        hmac_sha1(&[0x0b; 20], &[b"Hi There"]).to_vec(),
        unhex("b617318655057264e28bc0b6fb378c8ef146be00")
    );
    assert_eq!(
        hmac_sha1(b"Jefe", &[b"what do ya ", b"want for nothing?"]).to_vec(),
        unhex("effcdf6ae5eb2fa2d27416d5f184df9c259a7c79")
    );
    assert_eq!(
        hmac_sha1(&[0xaa; 20], &[&[0xdd; 50]]).to_vec(),
        unhex("125d7342b9ac11cd91a39af48aa17b4f63f175d3")
    );
    assert_eq!(
        hmac_sha1(
            &[0xaa; 80],
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
        )
        .to_vec(),
        unhex("aa4ae5e15272d00e95705637ce8a3b55ed402112")
    );
}

#[test]
fn pbkdf2_rfc6070() {
    let mut o = [0u8; 20];
    pbkdf2_hmac_sha1(b"password", b"salt", 1, &mut o);
    assert_eq!(o.to_vec(), unhex("0c60c80f961f0e71f3a9b524af6012062fe037a6"));
    pbkdf2_hmac_sha1(b"password", b"salt", 2, &mut o);
    assert_eq!(o.to_vec(), unhex("ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957"));
    pbkdf2_hmac_sha1(b"password", b"salt", 4096, &mut o);
    assert_eq!(o.to_vec(), unhex("4b007901b765489abead49d926f721d065a429c1"));
    let mut o25 = [0u8; 25];
    pbkdf2_hmac_sha1(
        b"passwordPASSWORDpassword",
        b"saltSALTsaltSALTsaltSALTsaltSALTsalt",
        4096,
        &mut o25,
    );
    assert_eq!(
        o25.to_vec(),
        unhex("3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038")
    );
}

#[test]
fn wpa_psk_annex_j() {
    assert_eq!(
        wpa_psk(b"password", b"IEEE").to_vec(),
        unhex("f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e")
    );
    assert_eq!(
        wpa_psk(b"ThisIsAPassword", b"ThisIsASSID").to_vec(),
        unhex("0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af")
    );
}

#[test]
fn prf_sha1_vectors() {
    let mut o = [0u8; 64];
    prf_sha1(&[0x0b; 20], b"prefix", b"Hi There", &mut o);
    assert_eq!(
        o.to_vec(),
        unhex("bcd4c650b30b9684951829e0d75f9d54b862175ed9f00606e17d8da35402ffee75df78c3d31e0f889f012120c0862beb67753e7439ae242edb8373698356cf5a")
    );
    prf_sha1(b"Jefe", b"prefix", b"what do ya want for nothing?", &mut o);
    assert_eq!(
        o.to_vec(),
        unhex("51f4de5b33f249adf81aeb713a3c20f4fe631446fabdfa58244759ae58ef9009a99abf4eac2ca5fa87e692c440eb40023e7babb206d61de7b92f41529092b8fc")
    );
}

#[test]
fn aes128_fips197() {
    let key: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
    let aes = Aes128::new(&key);
    let mut b: [u8; 16] = unhex("00112233445566778899aabbccddeeff").try_into().unwrap();
    aes.encrypt_block(&mut b);
    assert_eq!(b.to_vec(), unhex("69c4e0d86a7b0430d8cdb78070b4c55a"));
    aes.decrypt_block(&mut b);
    assert_eq!(b.to_vec(), unhex("00112233445566778899aabbccddeeff"));
}

#[test]
fn aes_key_wrap_rfc3394() {
    let kek: [u8; 16] = unhex("000102030405060708090A0B0C0D0E0F").try_into().unwrap();
    let plain = unhex("00112233445566778899AABBCCDDEEFF");
    let wrapped = aes_key_wrap(&kek, &plain);
    assert_eq!(
        wrapped,
        unhex("1FA68B0A8112B447AEF34BD8FB5A7B829D3E862371D2CFE5")
    );
    assert_eq!(aes_key_unwrap(&kek, &wrapped), Some(plain));

    for bit in [0usize, 70, 191] {
        let mut bad = wrapped.clone();
        bad[bit / 8] ^= 1 << (bit % 8);
        assert_eq!(aes_key_unwrap(&kek, &bad), None);
    }
    assert_eq!(aes_key_unwrap(&kek, &wrapped[..23]), None);
    assert_eq!(aes_key_unwrap(&kek, &wrapped[..16]), None);
    assert_eq!(aes_key_unwrap(&kek, &[]), None);
}

#[test]
fn ptk_derivation_symmetric_and_consistent() {
    let pmk = [0x42u8; 32];
    let aa = [0x02, 0, 0, 0, 0, 0x10];
    let spa = [0x02, 0, 0, 0, 0, 0x05];
    let mut anonce = [0u8; 32];
    let mut snonce = [0u8; 32];
    for i in 0..32 {
        anonce[i] = (i * 3 + 1) as u8;
        snonce[i] = (i * 5 + 200) as u8;
    }
    let a = derive_ptk(&pmk, &aa, &spa, &anonce, &snonce);
    let b = derive_ptk(&pmk, &spa, &aa, &snonce, &anonce);
    assert_eq!(a.kck, b.kck);
    assert_eq!(a.kek, b.kek);
    assert_eq!(a.tk, b.tk);

    // spa < aa and anonce < snonce here, so the ordered data is explicit.
    let mut data = Vec::new();
    data.extend_from_slice(&spa);
    data.extend_from_slice(&aa);
    data.extend_from_slice(&anonce);
    data.extend_from_slice(&snonce);
    let mut out = [0u8; 48];
    prf_sha1(&pmk, b"Pairwise key expansion", &data, &mut out);
    assert_eq!(a.kck[..], out[0..16]);
    assert_eq!(a.kek[..], out[16..32]);
    assert_eq!(a.tk[..], out[32..48]);
}

#[test]
fn rsn_ie_bytes() {
    assert_eq!(
        rsn_ie_psk_ccmp().to_vec(),
        unhex("30140100000fac040100000fac040100000fac020000")
    );
}

// ---------------------------------------------------------------------------
// Handshake simulation
// ---------------------------------------------------------------------------

const AP: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
const STA: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];

fn rc(n: u64) -> [u8; 8] {
    n.to_be_bytes()
}

fn gtk_kde(key_id: u8, gtk: &[u8; 16]) -> Vec<u8> {
    let mut v = vec![0xdd, 0x16, 0x00, 0x0f, 0xac, 0x01, key_id, 0x00];
    v.extend_from_slice(gtk);
    v
}

fn pad_key_data(mut d: Vec<u8>) -> Vec<u8> {
    d.push(0xdd);
    while d.len() % 8 != 0 || d.len() < 16 {
        d.push(0);
    }
    d
}

struct Authenticator {
    pmk: [u8; 32],
    anonce: [u8; 32],
    ptk: Option<Ptk>,
}

impl Authenticator {
    fn msg1(&self, counter: u64) -> Vec<u8> {
        build_eapol_key(
            2,
            2 | key_info::TYPE_PAIRWISE | key_info::ACK,
            16,
            &rc(counter),
            &self.anonce,
            &[],
        )
    }

    /// Verify message 2 and derive the PTK from its SNonce.
    fn on_msg2(&mut self, pdu: &[u8]) {
        let k = parse_eapol_key(pdu).expect("msg2 parses");
        assert_eq!(k.key_info & key_info::VERSION_MASK, 2);
        assert_eq!(
            k.key_info & !key_info::VERSION_MASK,
            key_info::TYPE_PAIRWISE | key_info::MIC
        );
        assert_eq!(k.key_data, &rsn_ie_psk_ccmp()[..]);
        let ptk = derive_ptk(&self.pmk, &AP, &STA, &self.anonce, &k.nonce);
        assert!(eapol_mic_ok(&ptk.kck, pdu), "msg2 MIC");
        self.ptk = Some(ptk);
    }

    fn msg3(&self, counter: u64, gtk: &[u8; 16], rsc: [u8; 8]) -> Vec<u8> {
        let ptk = self.ptk.as_ref().unwrap();
        let mut kd = rsn_ie_psk_ccmp().to_vec();
        kd.extend_from_slice(&gtk_kde(1, gtk));
        let wrapped = aes_key_wrap(&ptk.kek, &pad_key_data(kd));
        let mut m = build_eapol_key(
            2,
            2 | key_info::TYPE_PAIRWISE
                | key_info::INSTALL
                | key_info::ACK
                | key_info::MIC
                | key_info::SECURE
                | key_info::ENCRYPTED_KEY_DATA,
            16,
            &rc(counter),
            &self.anonce,
            &wrapped,
        );
        m[65..73].copy_from_slice(&rsc);
        eapol_set_mic(&ptk.kck, &mut m);
        m
    }

    fn group_msg1(&self, counter: u64, key_id: u8, gtk: &[u8; 16]) -> Vec<u8> {
        let ptk = self.ptk.as_ref().unwrap();
        let wrapped = aes_key_wrap(&ptk.kek, &pad_key_data(gtk_kde(key_id, gtk)));
        let mut m = build_eapol_key(
            2,
            2 | key_info::ACK
                | key_info::MIC
                | key_info::SECURE
                | key_info::ENCRYPTED_KEY_DATA,
            16,
            &rc(counter),
            &[0u8; 32],
            &wrapped,
        );
        eapol_set_mic(&ptk.kck, &mut m);
        m
    }

    fn check_reply(&self, pdu: &[u8], counter: u64, flags: u16) {
        let ptk = self.ptk.as_ref().unwrap();
        let k = parse_eapol_key(pdu).expect("reply parses");
        assert!(eapol_mic_ok(&ptk.kck, pdu), "reply MIC");
        assert_eq!(k.replay_counter, rc(counter));
        assert_eq!(k.key_info, 2 | flags);
        assert_eq!(k.nonce, [0u8; 32]);
        assert!(k.key_data.is_empty());
    }
}

fn setup() -> (Supplicant, Authenticator) {
    let pmk = wpa_psk(b"correct horse", b"LeandrOS-Net");
    let snonce = [0x5au8; 32];
    let mut anonce = [0u8; 32];
    for (i, b) in anonce.iter_mut().enumerate() {
        *b = (255 - i * 2) as u8;
    }
    let sup = Supplicant::new(pmk, STA, AP, rsn_ie_psk_ccmp().to_vec(), snonce);
    let auth = Authenticator {
        pmk,
        anonce,
        ptk: None,
    };
    (sup, auth)
}

fn expect_send(ev: &mut Vec<HsEvent>) -> Vec<u8> {
    match ev.remove(0) {
        HsEvent::Send(p) => p,
        _ => panic!("expected Send"),
    }
}

fn handshake_to_complete() -> (Supplicant, Authenticator, [u8; 16]) {
    let (mut sup, mut auth) = setup();
    let mut ev = sup.on_eapol(&auth.msg1(1));
    assert_eq!(ev.len(), 1);
    let msg2 = expect_send(&mut ev);
    auth.on_msg2(&msg2);
    assert!(!sup.is_complete());

    let gtk = [0x77u8; 16];
    let rsc = [1, 2, 3, 4, 5, 6, 0, 0];
    let mut ev = sup.on_eapol(&auth.msg3(2, &gtk, rsc));
    assert_eq!(ev.len(), 4);
    let msg4 = expect_send(&mut ev);
    auth.check_reply(
        &msg4,
        2,
        key_info::TYPE_PAIRWISE | key_info::MIC | key_info::SECURE,
    );
    match ev.remove(0) {
        HsEvent::InstallPtk(tk) => assert_eq!(tk, auth.ptk.as_ref().unwrap().tk),
        _ => panic!("expected InstallPtk"),
    }
    match ev.remove(0) {
        HsEvent::InstallGtk { key_id, key, rsc: r } => {
            assert_eq!(key_id, 1);
            assert_eq!(key, gtk.to_vec());
            assert_eq!(r, rsc);
        }
        _ => panic!("expected InstallGtk"),
    }
    assert!(matches!(ev.remove(0), HsEvent::Complete));
    assert!(sup.is_complete());
    let tk = auth.ptk.as_ref().unwrap().tk;
    (sup, auth, tk)
}

#[test]
fn full_handshake_and_rekey() {
    let (mut sup, auth, _tk) = handshake_to_complete();

    let new_gtk = [0x33u8; 16];
    let mut ev = sup.on_eapol(&auth.group_msg1(3, 2, &new_gtk));
    assert_eq!(ev.len(), 2);
    let reply = expect_send(&mut ev);
    auth.check_reply(&reply, 3, key_info::MIC | key_info::SECURE);
    match ev.remove(0) {
        HsEvent::InstallGtk { key_id, key, .. } => {
            assert_eq!(key_id, 2);
            assert_eq!(key, new_gtk.to_vec());
        }
        _ => panic!("expected InstallGtk"),
    }
}

#[test]
fn tampered_msg3_rejected() {
    let (mut sup, mut auth) = setup();
    let mut ev = sup.on_eapol(&auth.msg1(1));
    let msg2 = expect_send(&mut ev);
    auth.on_msg2(&msg2);

    let mut m3 = auth.msg3(2, &[0x77; 16], [0; 8]);
    let last = m3.len() - 1;
    m3[last] ^= 1;
    let ev = sup.on_eapol(&m3);
    assert!(ev.iter().all(|e| matches!(e, HsEvent::Error(_))));
    assert!(!ev.is_empty());
    assert!(!sup.is_complete());

    // The untampered message still completes afterwards.
    let ev = sup.on_eapol(&auth.msg3(2, &[0x77; 16], [0; 8]));
    assert!(matches!(ev.last(), Some(HsEvent::Complete)));
}

#[test]
fn replayed_messages_rejected_after_completion() {
    let (mut sup, auth, _) = handshake_to_complete();
    let ev = sup.on_eapol(&auth.msg3(2, &[0x77; 16], [0; 8]));
    assert_eq!(ev.len(), 1);
    assert!(matches!(ev[0], HsEvent::Error(_)));

    let ev = sup.on_eapol(&auth.group_msg1(2, 2, &[0x11; 16]));
    assert_eq!(ev.len(), 1);
    assert!(matches!(ev[0], HsEvent::Error(_)));
}

#[test]
fn retransmitted_msg3_is_reacknowledged_without_reinstall() {
    let (mut sup, auth, _) = handshake_to_complete();
    let ev = sup.on_eapol(&auth.msg3(3, &[0x77; 16], [0; 8]));
    assert_eq!(ev.len(), 1);
    assert!(matches!(ev[0], HsEvent::Send(_)));
}

#[test]
fn msg1_retransmit_restarts_with_new_anonce() {
    let (mut sup, mut auth) = setup();
    let mut ev = sup.on_eapol(&auth.msg1(1));
    let _ = expect_send(&mut ev);
    auth.anonce = [0x99; 32];
    let mut ev = sup.on_eapol(&auth.msg1(2));
    let msg2 = expect_send(&mut ev);
    auth.on_msg2(&msg2);
    let ev = sup.on_eapol(&auth.msg3(3, &[0x77; 16], [0; 8]));
    assert!(matches!(ev.last(), Some(HsEvent::Complete)));
}

#[test]
fn malformed_input_is_an_error() {
    let (mut sup, auth) = setup();
    assert!(parse_eapol_key(&[0u8; 10]).is_none());
    let m1 = auth.msg1(1);
    assert!(parse_eapol_key(&m1[..m1.len() - 1]).is_none());
    let ev = sup.on_eapol(&m1[..50]);
    assert!(matches!(ev[0], HsEvent::Error(_)));
    // Trailing link-layer padding is tolerated.
    let mut padded = m1.clone();
    padded.extend_from_slice(&[0u8; 7]);
    assert!(parse_eapol_key(&padded).is_some());
}

#[test]
fn gtk_kde_parsing() {
    let mut d = vec![0x30, 0x02, 0xaa, 0xbb];
    d.extend_from_slice(&gtk_kde(3, &[9; 16]));
    let g = find_gtk_kde(&d).unwrap();
    assert_eq!(g.key_id, 3);
    assert!(!g.tx);
    assert_eq!(g.key, vec![9u8; 16]);
    assert!(find_gtk_kde(&[0xdd, 0, 0, 0, 0, 0, 0, 0]).is_none());
    assert!(find_gtk_kde(&[0x30, 0x20, 1]).is_none());
}
