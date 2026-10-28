//! VHT TX rates in rate control: encoding, ladder, expected throughput.

use crate::iwl_assoc::{self as asc, ANT_A, ANT_AB};
use crate::iwl_rs::*;

fn vht(m2: Option<u8>, sgi80: bool) -> VhtRates { VhtRates { width: 80, max_mcs_1ss: 9, max_mcs_2ss: m2, sgi80 } }

fn cfg(v: Option<VhtRates>, mimo: bool, sgi: bool) -> RateCfg {
    RateCfg { ap_rates: vec![0x82, 0x84, 0x8b, 0x96, 12, 18, 24, 36, 48, 72, 96, 108], max_r500k: 0,
              valid_ant: ANT_AB, fixed_ant: None, band_24: false, rssi: Some(-50),
              ht: Some(HtRates { sgi, mimo, ht40: true, stbc: false, ldpc: false, vht: v }) }
}

#[test]
fn vht_rate_encoding() {
    let r = asc::vht_rate(9, 2, true, ANT_AB, 80);
    assert_eq!(r, asc::RATE_MCS_VHT | 9 | (1 << 4) | (2 << 11) | asc::RATE_MCS_SGI | (3 << 14));
    let r = asc::vht_rate(0, 1, false, ANT_A, 20);
    assert_eq!(r, asc::RATE_MCS_VHT | (1 << 14));
    assert_eq!(asc::rate_vht(asc::vht_rate(5, 2, false, ANT_AB, 40)), Some((5, 2, 40)));
}

#[test]
fn vht_rate_id_round_trip() {
    for &w in &[20u16, 40, 80] {
        for nss in 1..=2u8 {
            for mcs in 0..=9u8 {
                for &sgi in &[false, true] {
                    if !vht_valid(mcs, w) { continue; }
                    let id = vht_id(mcs, nss, sgi, w);
                    assert!(is_vht(id) && is_ht(id) && !is_40(id));
                    assert_eq!(is_mimo_id(id), nss == 2);
                    let code = rate_code(id, ANT_A);
                    assert_eq!(rate_id(code), Some(id), "{}", rate_label(id));
                    assert_eq!(rate_id(code | (3 << 14)), Some(id));
                    assert!(expected_tpt(id) > 0 && expected_tpt_agg(id) >= expected_tpt(id) / 2, "{}", rate_label(id));
                }
            }
        }
    }
    assert_eq!(rate_label(vht_id(9, 2, true, 80)), "VHT9x2/80s");
    assert_eq!(rate_label(vht_id(0, 1, false, 20)), "VHT0x1/20");
    // HT and legacy ids are unchanged.
    assert_eq!(ht_id(7, false), 0x87);
    assert_eq!(rate_id(asc::ht_rate_w(15, true, ANT_AB, true)), Some(ht_id_w(15, true, true)));
    assert_eq!(rate_id(asc::legacy_rate(108).unwrap()), Some(108));
}

#[test]
fn vht_tables_and_nominal() {
    assert_eq!(expected_tpt_agg(vht_id(9, 2, true, 80)), 5143);
    assert_eq!(expected_tpt_agg(vht_id(9, 1, false, 80)), 2466);
    assert_eq!(expected_tpt(vht_id(0, 1, false, 80)), 130);
    assert_eq!(expected_tpt(vht_id(7, 1, false, 40)), expected_tpt(ht_id_w(7, false, true)));
    assert_eq!(nominal_r500k(vht_id(9, 1, false, 80)), 780);
    assert_eq!(nominal_r500k(vht_id(7, 2, false, 40)), 540);
}

#[test]
fn vht_ladder_80() {
    let c = RateCtl::new(&cfg(Some(vht(Some(9), false)), true, false));
    let first_vht = c.rates.iter().position(|&r| is_vht(r)).unwrap();
    assert!(c.rates[..first_vht].iter().all(|&r| !is_ht(r)), "legacy below VHT");
    assert!(c.rates[first_vht..].iter().all(|&r| is_vht(r) && vht_width(r) == 80));
    assert!(c.rates.iter().all(|&r| !(is_vht(r) && vht_mcs(r) == 9 && vht_width(r) == 20)));
    assert_eq!(vht_mcs(c.rates[first_vht]), 0);
    // -50 dBm: the best single-stream MCS (80 MHz: -6 dB still above -60).
    assert!(is_vht(c.rates[c.cur]));
    assert!(c.table()[0] & asc::RATE_MCS_VHT != 0);
    assert!(c.cur_label().starts_with("VHT"));
}

#[test]
fn vht_mimo_ladder_is_monotonic() {
    let mut c = RateCtl::new(&cfg(Some(vht(Some(9), false)), false, false));
    c.enable_col_search(false, true);
    assert!(c.cols.iter().any(|k| k.mimo));
    for col in &c.cols.clone() {
        let mut k = RateCtl::new(&cfg(Some(vht(Some(9), false)), col.mimo, col.sgi));
        k.enable_col_search(false, true);
        let t: Vec<u32> = k.rates.iter().map(|&r| expected_tpt(r)).collect();
        assert!(t.windows(2).all(|w| w[0] <= w[1]), "{:?} {:?}", col, t);
        if col.mimo { assert!(k.rates.iter().any(|&r| vht_nss(r) == 2 && is_vht(r))); }
    }
    let m = RateCtl::new(&cfg(Some(vht(Some(9), false)), true, false));
    let lq = m.lq(1);
    let t = m.table();
    assert_eq!(RateCtl::mimo_delim(&t) > 0, vht_nss(m.rates[m.cur]) == 2 && is_vht(m.rates[m.cur]));
    assert_eq!(lq.len(), m.lq(1).len());
    // No 2-stream rates when the AP has one stream.
    let s = RateCtl::new(&cfg(Some(vht(None, false)), true, false));
    assert!(s.rates.iter().all(|&r| !is_vht(r) || vht_nss(r) == 1));
}

#[test]
fn vht_mimo_delim_sees_two_streams() {
    let mut t = [asc::vht_rate(3, 1, false, ANT_A, 80); 16];
    t[0] = asc::vht_rate(1, 2, false, ANT_AB, 80);
    t[1] = t[0];
    assert_eq!(RateCtl::mimo_delim(&t), 2);
}

#[test]
fn vht_ampdu_counts_and_climbs() {
    let mut c = RateCtl::new(&cfg(Some(vht(None, false)), false, false));
    c.mark_sent(0);
    c.set_agg(true, 63);
    c.mark_sent(0);
    let start = c.cur;
    let mut now = 200;
    for _ in 0..40 {
        let r = c.table()[0];
        c.on_ampdu(r, 16, 16, now);
        if c.due(now) { c.mark_sent(now); }
        now += 120;
    }
    assert_eq!(c.mismatched, 0);
    assert!(c.ampdus > 0);
    assert!(c.cur > start || start == c.rates.len() - 1, "{} -> {}", start, c.cur);
    let before = c.mismatched;
    c.on_ampdu(asc::vht_rate(2, 1, false, ANT_A, 40), 8, 8, now);
    assert_eq!(c.mismatched, before + 1);
}

#[test]
fn no_vht_is_the_ht_path() {
    let a = RateCtl::new(&cfg(None, true, true));
    let b = RateCtl::new(&cfg(None, true, true));
    assert_eq!(a.lq(1), b.lq(1));
    assert!(a.rates.iter().all(|&r| !is_vht(r) && r < 0x100));
    assert_eq!(a.table()[0] & asc::RATE_MCS_VHT, 0);
    // The HT40 table keeps its known entry point and chain.
    assert!(a.rates.iter().any(|&r| is_40(r)));
}
