//! Rate-control column search (rs.c rs_switch_to_column): SISO / MIMO2 and
//! long / short guard interval, kept on a clear expected-throughput win.

use crate::iwl_assoc::*;
use crate::iwl_rs::*;

/// The laptop's link: 5 GHz, 40 MHz, STBC + LDPC, both TX chains, -77 dBm.
fn ctl(valid_ant: u8, fixed: Option<u8>) -> RateCtl {
    let cfg = RateCfg { ap_rates: vec![0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c], max_r500k: 0, valid_ant, fixed_ant: fixed,
                        band_24: false, rssi: Some(-77), ht: Some(HtRates { sgi: false, mimo: false, ht40: true, stbc: true, ldpc: true, vht: None }) };
    let mut c = RateCtl::new(&cfg);
    c.set_agg(true, 63);
    c.mark_sent(0);
    c
}

/// A-MPDUs of 24 subframes every 20 ms for `secs` s; subframe success
/// permille per rate id from `p`. Returns goodput (acked subframes x
/// nominal rate) over the last `tail` seconds.
fn run(c: &mut RateCtl, now: &mut u64, secs: u64, tail: u64, p: &dyn Fn(u16) -> u64) -> u64 {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut good = 0;
    for k in 0..secs * 50 {
        *now += 20;
        if c.due(*now) { c.mark_sent(*now); }
        let r = c.sent.unwrap()[0];
        let id = rate_id(r).unwrap();
        let mut acked = 0u8;
        for _ in 0..24 {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            if x % 1000 < p(id) { acked += 1; }
        }
        if k >= (secs - tail) * 50 { good += acked as u64 * nominal_r500k(id) as u64; }
        c.on_ampdu(r, 24, acked, *now);
    }
    good
}

fn mcs(id: u16) -> u8 { (id & 0x0f) as u8 }
fn sgi(id: u16) -> bool { id & HT_SGI != 0 }

#[test]
fn col_search_capability_checks() {
    let mut c = ctl(ANT_AB, None);
    c.enable_col_search(true, true);
    assert_eq!(c.cols.len(), 4);
    assert_eq!(c.cur_col(), Col { mimo: false, sgi: false });
    let mut c = ctl(ANT_AB, None);
    c.enable_col_search(false, true);
    assert_eq!(c.cols, vec![Col { mimo: false, sgi: false }, Col { mimo: true, sgi: false }]);
    // One TX chain, or one forced antenna: no two-stream column.
    for (v, f) in [(ANT_A, None), (ANT_AB, Some(ANT_B))] {
        let mut c = ctl(v, f);
        c.enable_col_search(true, true);
        assert!(c.cols.iter().all(|k| !k.mimo), "valid {v:#x} fixed {f:?}");
        assert_eq!(c.cols.len(), 2);
    }
    // No HT: nothing to search.
    let cfg = RateCfg { ap_rates: vec![0x8c, 0x18, 0x30, 0x6c], max_r500k: 0, valid_ant: ANT_AB, fixed_ant: None,
                        band_24: false, rssi: Some(-60), ht: None };
    let mut c = RateCtl::new(&cfg);
    c.enable_col_search(true, true);
    assert_eq!(c.cols.len(), 1);
}

#[test]
fn mimo2_column_ladder_has_low_two_stream_mcs() {
    let mut c = ctl(ANT_AB, None);
    c.enable_col_search(false, true);
    let mut now = 0;
    // Everything delivers: the search reaches the MIMO2 column and keeps it.
    run(&mut c, &mut now, 30, 1, &|_| 950);
    assert!(c.cur_col().mimo, "{}", c.summary(now));
    let ht: Vec<u8> = c.rates.iter().copied().filter(|&r| is_ht(r)).map(mcs).collect();
    assert_eq!(ht, vec![0, 8, 9, 10, 11, 12, 13, 14, 15]);
    // MIMO2 MCS go out on both chains without STBC; the chain's SISO part keeps STBC.
    let t = c.table();
    assert_eq!((t[0] & RATE_MCS_ANT_MSK) >> RATE_MCS_ANT_POS, ANT_AB as u32);
    assert_eq!(t[0] & RATE_MCS_STBC, 0);
}

/// -77 dBm, the 'Anvara 5G' position: SISO MCS 0-3 deliver 75 %, MCS 4+
/// under 45 %; two streams deliver well up to MCS 10.
#[test]
fn col_search_switches_to_mimo2_when_it_delivers_more() {
    let chan = |id: u16| -> u64 {
        match mcs(id) { 0..=3 => 750, 4..=7 => 400, 8..=10 => 900, _ => 300 }
    };
    let mut base = ctl(ANT_AB, None);
    let mut now = 0;
    let g0 = run(&mut base, &mut now, 60, 20, &chan);
    let mut c = ctl(ANT_AB, None);
    c.enable_col_search(true, true);
    let mut now = 0;
    let g1 = run(&mut c, &mut now, 60, 20, &chan);
    assert!(c.cur_col().mimo && c.col_kept >= 1, "{}", c.summary(now));
    assert!(g1 * 100 >= g0 * 130, "goodput {g1} vs SISO only {g0}");
}

/// Two streams and short GI both deliver worse: the search probes, goes
/// back each time, and backs off (the probes cost under 10 % goodput).
#[test]
fn col_search_reverts_and_backs_off_when_columns_are_worse() {
    let chan = |id: u16| -> u64 {
        if mcs(id) >= 8 { 150 } else if sgi(id) { 350 } else if mcs(id) <= 3 { 750 } else { 400 }
    };
    let mut base = ctl(ANT_AB, None);
    let mut now = 0;
    let g0 = run(&mut base, &mut now, 180, 170, &chan);
    let mut c = ctl(ANT_AB, None);
    c.enable_col_search(true, true);
    let mut now = 0;
    let g1 = run(&mut c, &mut now, 180, 170, &chan);
    assert_eq!(c.cur_col(), Col { mimo: false, sgi: false }, "{}", c.summary(now));
    assert_eq!(c.col_kept, 0);
    assert!(c.col_probes >= 3 && c.col_probes <= 12, "{}", c.summary(now));
    assert!(g1 * 100 >= g0 * 90, "goodput {g1} vs no search {g0}");
}

/// Short GI delivers as well as long GI: ~10 % more per MCS, kept.
#[test]
fn col_search_keeps_short_gi_when_it_delivers_as_well() {
    let chan = |id: u16| -> u64 { if mcs(id) >= 8 { 100 } else if mcs(id) <= 3 { 750 } else { 400 } };
    let mut c = ctl(ANT_AB, None);
    c.enable_col_search(true, true);
    let mut now = 0;
    run(&mut c, &mut now, 60, 1, &chan);
    assert_eq!(c.cur_col(), Col { mimo: false, sgi: true }, "{}", c.summary(now));
    assert!(c.summary(now).contains("col SISO-SGI"));
}
