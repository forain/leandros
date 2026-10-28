//! VHT / 80 MHz: our VHT capabilities, the AP's VHT operation, the 80 MHz
//! PHY context, station flags and VHT receive rates.

use crate::iwl_assoc::*;
use crate::iwl_cmd::*;
use crate::wifi_ht::*;

#[test]
fn vht_cap_ie_matches_8265() {
    let c = iwl8265_ht_cfg(3, 3);
    let e = vht_cap_ie(&c);
    assert_eq!((e[0], e[1]), (191, 12));
    let cap = u32::from_le_bytes([e[2], e[3], e[4], e[5]]);
    // SGI80 | RXLDPC | TXSTBC | RXSTBC_1 | A-MPDU exponent 7, 3895-byte MPDUs, no 160, no beamforming.
    assert_eq!(cap, 0x0380_01b0);
    // MCS 0-9 on two streams, both directions; highest rates 0.
    assert_eq!(&e[6..14], &[0xfa, 0xff, 0, 0, 0xfa, 0xff, 0, 0]);
    let p = parse_vht_cap(&e[2..]).unwrap();
    assert_eq!((p.rx_streams(), p.ampdu_exp(), p.sgi80()), (2, 7, true));
    assert_eq!(vht_map_max_mcs(p.rx_mcs, 2), Some(9));
    let one = vht_cap_ie(&iwl8265_ht_cfg(1, 1));
    assert_eq!(&one[6..8], &[0xfe, 0xff]);
    assert_eq!(u32::from_le_bytes([one[2], one[3], one[4], one[5]]) & vhtcap::TXSTBC, 0);
}

#[test]
fn vht_op_center80() {
    // 'Anvara 5G': primary 40, 80 MHz centred on 42.
    let op = parse_vht_op(&[1, 42, 0, 0xfc, 0xff]).unwrap();
    assert_eq!(op.center80(40), Some(42));
    assert_eq!(op.center80(36), Some(42));
    assert_eq!(op.center80(48), Some(42));
    assert_eq!(op.center80(52), None);
    // 20/40 only.
    assert_eq!(VhtOp { width: 0, seg0: 0, seg1: 0 }.center80(40), None);
    // 160 MHz, new encoding (seg0 = primary 80, seg1 = 160 centre) and the old one.
    assert_eq!(VhtOp { width: 1, seg0: 42, seg1: 50 }.center80(44), Some(42));
    assert_eq!(VhtOp { width: 2, seg0: 50, seg1: 0 }.center80(60), Some(58));
    assert_eq!(VhtOp { width: 2, seg0: 50, seg1: 0 }.center80(36), Some(42));
    assert_eq!(channels80(42), [36, 40, 44, 48]);
    let ies = [&[191u8, 12][..], &vht_cap_ie(&iwl8265_ht_cfg(3, 3))[2..], &[192, 5, 1, 42, 0, 0xfc, 0xff]].concat();
    let b = parse_bss_ies(&ies);
    assert!(b.vht_cap.is_some());
    assert_eq!(b.vht_op.unwrap().seg0, 42);
}

#[test]
fn phy_ctxt_80_ctrl_pos() {
    assert_eq!(ctrl_pos80(36, 42), Some(1));
    assert_eq!(ctrl_pos80(40, 42), Some(0));
    assert_eq!(ctrl_pos80(44, 42), Some(4));
    assert_eq!(ctrl_pos80(48, 42), Some(5));
    assert_eq!(ctrl_pos80(52, 42), None);
    let p = phy_ctxt_cmd_80(0, 0, 2, 40, 42, 3, 3).unwrap();
    let w = phy_ctxt_cmd_w(0, 0, 2, 40, Some(1), 3, 3);
    assert_eq!(p.len(), w.len());
    // band 5 GHz (PHY_BAND_5), channel 40, width 80 (2), control 1_BELOW (0).
    assert_eq!(&p[16..20], &[PHY_BAND_5, 40, 2, 0]);
    assert_eq!(&p[20..], &w[20..]);
    assert!(phy_ctxt_cmd_80(0, 0, 2, 52, 42, 3, 3).is_none());
}

#[test]
fn sta_flags_vht80() {
    let (f, m) = sta_vht80_flags(2, 2, 3, 3, 7, 5);
    assert_eq!(f & STA_FLG_FAT_EN_MSK, STA_FLG_FAT_EN_80MHZ);
    assert_eq!(f & STA_FLG_MIMO_EN_MSK, STA_FLG_MIMO_EN_MIMO2);
    assert_eq!((f & STA_FLG_MAX_AGG_SIZE_MSK) >> STA_FLG_MAX_AGG_SIZE_SHIFT, 7);
    assert_eq!((f & STA_FLG_AGG_MPDU_DENS_MSK) >> STA_FLG_AGG_MPDU_DENS_SHIFT, 5);
    assert_eq!(m & STA_FLG_FAT_EN_MSK, STA_FLG_FAT_EN_MSK);
    // A single-stream VHT AP on an HT 2-stream cap still gets MIMO from HT; 1/1 gets none.
    assert_eq!(sta_vht80_flags(1, 1, 3, 3, 7, 5).0 & STA_FLG_MIMO_EN_MSK, 0);
}

#[test]
fn vht_rx_rate_is_not_legacy() {
    // VHT MCS5, 1 stream, 80 MHz: low byte 0x05 is also 12M OFDM's PLCP.
    let r = RATE_MCS_VHT | RATE_MCS_CHAN_WIDTH_80 | 5;
    assert_eq!(rate_vht(r), Some((5, 1, 80)));
    assert_eq!(rate_r500k(r), None);
    assert_eq!(rate_vht(RATE_MCS_VHT | 0x19), Some((9, 2, 20)));
    assert_eq!(rate_vht(0x05), None);
    assert_eq!(rate_r500k(0x05), Some(24));
}
