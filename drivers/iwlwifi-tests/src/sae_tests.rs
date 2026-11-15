//! WPA3-Personal SAE: P-256 known answers (NIST, RFC 5903, RFC 9380 SSWU) and
//! full exchanges, hunt-and-peck and hash-to-element.

use crate::wifi_sae::*;

fn rng() -> impl FnMut(&mut [u8]) {
    let mut st = 0x1234_5678_9abc_def0u64;
    move |b: &mut [u8]| {
        for x in b.iter_mut() {
            st = st.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *x = (st >> 33) as u8;
        }
    }
}

#[test]
fn sae_selftest() {
    let mut r = rng();
    assert_eq!(selftest(&mut r), Ok(()));
}

#[test]
fn auth_frames() {
    let mut r = rng();
    let pwe = pwe_hunt_and_peck(b"pw", &[1; 6], &[2; 6], None);
    let s = Sae::new(pwe, &mut r);
    let f = commit_frame(0, &[], &s.commit_body());
    let a = parse_auth(&f).unwrap();
    assert_eq!((a.seq, a.status), (1, 0));
    assert_eq!(a.rest.len(), 2 + 96);
    let t = anti_clogging_frame(&make_token(&[7; 32], &[1; 6]));
    assert_eq!(parse_auth(&t).unwrap().status, STATUS_ANTI_CLOGGING);
}

#[test]
fn bip_and_sa_query() {
    use crate::wifi_crypto::*;
    let k = [9u8; 16];
    let (a1, a2) = ([0xff; 6], [1, 2, 3, 4, 5, 6]);
    let body = [0x03u8, 0x00]; // reason 3 stub
    let fc = 0x00c0;
    let p = bip_protect(&k, 4, 5, fc, &a1, &a2, &a2, &body);
    let mut last = 0;
    assert!(bip_verify(&k, 4, &mut last, fc, &a1, &a2, &a2, &p));
    assert_eq!(last, 5);
    assert!(!bip_verify(&k, 4, &mut last, fc, &a1, &a2, &a2, &p), "replay");
    let mut t = p.clone(); t[0] ^= 1;
    let mut l2 = 0;
    assert!(!bip_verify(&k, 4, &mut l2, fc, &a1, &a2, &a2, &t), "tampered");
    assert!(!bip_verify(&[8; 16], 4, &mut l2, fc, &a1, &a2, &a2, &p), "wrong key");
    assert_eq!(sa_query_response(&sa_query_frame(false, 0x1234)), Some(sa_query_frame(true, 0x1234)));
    let ie = rsn_ie_build(0x000f_ac04, 8, RSN_CAP_MFPC | RSN_CAP_MFPR, Some(&[1; 16]));
    assert_eq!(ie[1] as usize, ie.len() - 2);
    assert_eq!(ie.len(), 2 + 2 + 4 + 6 + 6 + 2 + 2 + 16 + 4);
}
