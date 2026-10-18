//! 40 MHz: our HT capabilities, the AP's HT operation, the PHY context,
//! station flags and rate control at 40 MHz.

use crate::iwl_assoc::*;
use crate::iwl_cmd::*;
use crate::iwl_rs::*;
use crate::wifi_ht::*;

#[test]
fn ht40_cap_op_and_phy() {
    // 40 MHz advertised: Supported Channel Width Set + SGI 40, 150 Mb/s per stream.
    let mut c = iwl8265_ht_cfg(3, 3);
    c.ht40 = true;
    let ie = ht_cap_ie(&c);
    let cap = u16::from_le_bytes([ie[2], ie[3]]);
    assert_eq!(cap & (htcap::SUP_WIDTH_20_40 | htcap::SGI_40), htcap::SUP_WIDTH_20_40 | htcap::SGI_40);
    assert_eq!(u16::from_le_bytes([ie[15], ie[16]]), 300);
    let p = parse_ht_cap(&ie[2..]).unwrap();
    assert!(p.sup_40() && p.sgi40());
    assert!(!parse_ht_cap(&ht_cap_ie(&iwl8265_ht_cfg(3, 3))[2..]).unwrap().sup_40());
    // HT operation: ht_param secondary offset (1 above, 3 below) + STA width bit.
    let op = |param: u8, mode: u16| HtOp { primary: 40, ht_param: param, op_mode: mode };
    assert_eq!(op(0x05, 0).ht40_offset(), Some(1));
    assert_eq!(op(0x07, 0).ht40_offset(), Some(-1));
    assert_eq!(op(0x03, 0).ht40_offset(), None, "STA channel width 20 only");
    assert_eq!(op(0x04, 0).ht40_offset(), None, "no secondary channel");
    assert_eq!(secondary_channel(40, -1), 36);
    assert_eq!(secondary_channel(36, 1), 40);
    // 20 MHz protection mode protects 40 MHz transmissions only.
    assert!(!op(0x07, 2).needs_ht_prot(false));
    assert!(op(0x07, 2).needs_ht_prot(true));
    assert!(op(0x07, 3).needs_ht_prot(false));
    // PHY context: width 40 and the control position (secondary below = ABOVE).
    let below = phy_ctxt_cmd_w(0, 0, FW_CTXT_ACTION_MODIFY, 40, Some(-1), 3, 3);
    assert_eq!(&below[16..20], &[PHY_BAND_5, 40, PHY_CHANNEL_MODE40, PHY_CTRL_POS_ABOVE]);
    let above = phy_ctxt_cmd_w(0, 0, FW_CTXT_ACTION_MODIFY, 36, Some(1), 3, 3);
    assert_eq!(&above[16..20], &[PHY_BAND_5, 36, PHY_CHANNEL_MODE40, 0]);
    assert_eq!(phy_ctxt_cmd(0, 0, FW_CTXT_ACTION_MODIFY, 36, 3, 3), phy_ctxt_cmd_w(0, 0, FW_CTXT_ACTION_MODIFY, 36, None, 3, 3));
    let (f, _) = sta_ht_flags_w(2, 3, 3, 0, true);
    assert_eq!(f & STA_FLG_FAT_EN_MSK, STA_FLG_FAT_EN_40MHZ);
    assert_eq!(sta_ht_flags_w(2, 3, 3, 0, false), sta_ht_flags(2, 3, 3, 0));
}

#[test]
fn ht40_rates() {
    let id = ht_id_w(7, false, true);
    assert!(is_40(id) && !is_40(ht_id(7, false)));
    let r = rate_code(id, ANT_A);
    assert_eq!(r, ht_rate(7, false, ANT_A) | RATE_MCS_CHAN_WIDTH_40);
    assert!(rate_is_40(r));
    assert_eq!(rate_id(r), Some(id));
    assert_eq!(rate_label(id), "MCS7/40");
    assert_eq!(expected_tpt(id), 257);
    assert_eq!(expected_tpt_agg(id), 911);
    assert_eq!(nominal_r500k(id), 270);
    let cfg = RateCfg { ap_rates: vec![0x8c, 18, 0x98, 36, 0xb0, 72, 96, 108], max_r500k: 0, valid_ant: ANT_AB, fixed_ant: None,
                        band_24: false, rssi: Some(-76), ht: Some(HtRates { sgi: false, mimo: false, ht40: true }) };
    let c = RateCtl::new(&cfg);
    assert!(c.rates.iter().filter(|&&r| is_ht(r)).all(|&r| is_40(r)));
    // -76 dBm at 40 MHz counts as -79: MCS 3.
    assert_eq!(c.cur_label(), "MCS3/40");
    assert!(rate_is_40(c.table()[0]));
}
