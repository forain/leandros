//! TKIP receive path for WPA/WPA2 mixed-mode networks (group TKIP): key
//! mixing and Michael against the IEEE 802.11-2012 M.6.3 test vector (the
//! expected values were produced by hostapd's independent wlantest/tkip.c
//! and its MIC equals the standard's 68 81 a3 f3 d6 48 d0 3c), RC4/CRC-32
//! known answers, and the firmware ADD_STA_KEY / RX status encoding as
//! iwlmvm lays them out (ADD_MODIFY_STA_KEY_API_S_VER_1).

use crate::iwl_assoc::*;
use crate::iwl_cmd::*;
use crate::wifi_crypto::*;
use alloc::vec::Vec;

fn unhex(s: &str) -> Vec<u8> {
    let s: Vec<u8> = s.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    s.chunks(2).map(|c| u8::from_str_radix(core::str::from_utf8(c).unwrap(), 16).unwrap()).collect()
}

const TK32: [u8; 32] = [
    0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78, 0x90, 0x12,
    0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34,
];
const HDR: &str = "08422c00020304050608020304050607020304050607d002";
const PLAIN: &str = "aaaa03000000080045000054000040004001a555c0a80a02c0a80a0108003ab000000000cd4c050000000000\
                     08090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f3031323334353637";
const ENC: &str = "c00e14fce7cfabc77547e666e57c0dac704a1e358a88c11c8e2e282e3801027a4656055ee93e9c254702e9735805ddb5\
                   769ba73f1ebb56e844ef912285d3dd6e541e823873558adba079068abd7f7f50959675acc4b4de9aa99c05f289a7c52fee5bfc14f6f8e5f8";

fn tk() -> [u8; 16] {
    TK32[..16].try_into().unwrap()
}

#[test]
fn phase1_phase2_vectors() {
    let ta = [2, 3, 4, 5, 6, 7];
    let p1 = tkip_phase1(&tk(), &ta, 0);
    assert_eq!(p1, [0x58bb, 0x1f07, 0x939e, 0x38b4, 0x4b25]);
    assert_eq!(tkip_phase2(&tk(), &p1, 1).to_vec(), unhex("0020014cfe67bed27c867b1bf8028b1c"));
    let p2 = tkip_phase1(&tk(), &[0x10, 0x22, 0x33, 0x44, 0x55, 0x66], 0x1234_5678);
    assert_eq!(p2, [0x62ea, 0xc85a, 0x35bd, 0xe5eb, 0x0831]);
    assert_eq!(tkip_phase2(&tk(), &p2, 0xabcd).to_vec(), unhex("ab2bcd79b66bc6dc90f114158a3ce10a"));
}

#[test]
fn rc4_and_crc32_known_answers() {
    let mut d = *b"Plaintext";
    rc4(b"Key", &mut d);
    assert_eq!(d.to_vec(), unhex("bbf316e8d940af0ad3"));
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
}

#[test]
fn michael_m63_vector() {
    let hdr = unhex(HDR);
    let plain = unhex(PLAIN);
    let key: [u8; 8] = TK32[16..24].try_into().unwrap();
    // FromDS: DA = addr1, SA = addr3; non-QoS: priority 0.
    let mic = michael(&key, &hdr[4..10], &hdr[16..22], 0, &plain);
    assert_eq!(mic.to_vec(), unhex("6881a3f3d648d03c"));
    // The raw function over the same pseudo header + data agrees.
    let mut ph = Vec::new();
    ph.extend_from_slice(&hdr[4..10]);
    ph.extend_from_slice(&hdr[16..22]);
    ph.extend_from_slice(&[0, 0, 0, 0]);
    assert_eq!(michael_raw(&key, &[&ph, &plain]), mic);
}

/// The whole receive path in software: IV -> TSC, phase 1/2, RC4, ICV,
/// then Michael over the decrypted MSDU.
#[test]
fn decrypt_m63_mpdu_and_check_michael() {
    let iv = unhex("0020012000000000");
    assert_eq!(tkip_tsc(&iv), Some((1, 0)));
    assert_eq!(tkip_tsc(&unhex("0020010000000000")), None, "ExtIV must be set");
    assert_eq!(tkip_tsc(&unhex("12345660cdab0000")), Some((0xabcd_1256, 1)));
    let (tsc, _) = tkip_tsc(&iv).unwrap();
    let ta: [u8; 6] = unhex(HDR)[10..16].try_into().unwrap();
    let p1 = tkip_phase1(&tk(), &ta, (tsc >> 16) as u32);
    let mut body = unhex(ENC);
    assert!(tkip_decrypt(&tk(), &p1, tsc as u16, &mut body));
    let plain = unhex(PLAIN);
    assert_eq!(&body[..plain.len()], &plain[..]);
    assert_eq!(body[plain.len()..plain.len() + 8].to_vec(), unhex("6881a3f3d648d03c"));
    // As the firmware hands it over: header | IV | plaintext | MIC | ICV.
    let mut frame = unhex(HDR);
    frame.extend_from_slice(&iv);
    frame.extend_from_slice(&body);
    let rx_mic: [u8; 8] = TK32[16..24].try_into().unwrap();
    assert!(tkip_frame_mic_ok(&rx_mic, &frame, 24, 4));
    assert!(tkip_frame_mic_ok(&rx_mic, &frame[..frame.len() - 4], 24, 0));
    // A flipped payload bit or the wrong Michael key fails.
    let mut bad = frame.clone();
    bad[24 + 8 + 20] ^= 1;
    assert!(!tkip_frame_mic_ok(&rx_mic, &bad, 24, 4));
    assert!(!tkip_frame_mic_ok(&TK32[24..32].try_into().unwrap(), &frame, 24, 4));
    // A wrong IV16 garbles the keystream: the ICV catches it.
    let mut body2 = unhex(ENC);
    assert!(!tkip_decrypt(&tk(), &p1, 2, &mut body2));
}

/// ADD_STA_KEY v1 for a TKIP group key, field by field as iwlmvm's
/// iwl_mvm_send_sta_key fills struct iwl_mvm_add_sta_key_cmd_v1.
#[test]
fn add_sta_key_tkip_layout() {
    let k = TkipKey::from_gtk(&TK32).unwrap();
    let p1k = [0x58bb, 0x1f07, 0x939e, 0x38b4, 0x4b25];
    let c = add_sta_key_tkip(1, 2, 1, &k.driver_layout(), true, 0x0001_02ab, &p1k);
    assert_eq!(c.len(), 64);
    // sta 1, offset 2, flags = TKIP(3) | WEP_KEY_MAP | keyid 1 << 8 | MULTICAST
    assert_eq!(&c[0..4], &[1, 2, 0x0b, 0x41]);
    assert_eq!(&c[4..20], &TK32[..16]);
    assert_eq!(&c[20..28], &TK32[24..32], "station TX MIC key at 16");
    assert_eq!(&c[28..36], &TK32[16..24], "station RX MIC key at 24");
    assert_eq!(&c[36..52], &[0u8; 16], "rx_secur_seq_cnt");
    assert_eq!(c[52], 0xab, "tkip_rx_tsc_byte2 = low byte of IV32");
    assert_eq!(c[53], 0);
    let ttak: Vec<u16> = c[54..64].chunks(2).map(|w| u16::from_le_bytes([w[0], w[1]])).collect();
    assert_eq!(ttak, p1k.to_vec());
    // The CCMP encoder is unchanged.
    assert_eq!(&add_sta_key_ccmp(1, 1, 2, &[0xbb; 16], true)[0..4], &[1, 1, 0x0a, 0x42]);
}

/// REPLY_RX_MPDU status for TKIP (iwlmvm rx.c): decrypted when ICV_OK and
/// TTAK_OK; the 8-byte IV stays in front, Michael (8) + ICV (4) behind.
#[test]
fn rx_status_tkip() {
    let ok = RX_MPDU_STATUS_SEC_TKIP | RX_MPDU_STATUS_ICV_OK | RX_MPDU_STATUS_TTAK_OK;
    assert_eq!(RX_MPDU_STATUS_SEC_TKIP, 3 << 8);
    assert_eq!(RX_MPDU_STATUS_TTAK_OK, 1 << 7);
    assert_eq!(rx_crypto_lengths(0x4208, ok), Some((8, 12)));
    assert_eq!(rx_crypto_lengths(0x4208, ok & !RX_MPDU_STATUS_ICV_OK), None);
    assert_eq!(rx_crypto_lengths(0x4208, ok & !RX_MPDU_STATUS_TTAK_OK), None);
    assert_eq!(rx_crypto_lengths(0x4208, RX_MPDU_STATUS_SEC_CCM | RX_MPDU_STATUS_MIC_OK), Some((8, 8)));
}
