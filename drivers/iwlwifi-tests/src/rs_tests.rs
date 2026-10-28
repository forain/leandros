use crate::iwl_assoc::*;
use crate::iwl_rs::*;

fn u32_at(v: &[u8], o: usize) -> u32 { u32::from_le_bytes([v[o], v[o + 1], v[o + 2], v[o + 3]]) }

/// A TX_RSP v3 (single frame) as the firmware sends it.
fn tx_rsp_bytes(status: u16, failure_frame: u8, rate: u32, bt_kill: u8, rts: u8, tpc: u8, ssn: u16) -> Vec<u8> {
    let mut r = vec![0u8; 44];
    r[0] = 1;
    r[1] = bt_kill;
    r[2] = rts;
    r[3] = failure_frame;
    r[4..8].copy_from_slice(&rate.to_le_bytes());
    r[22] = tpc;
    r[28..30].copy_from_slice(&0x1230u16.to_le_bytes());
    r[36..38].copy_from_slice(&status.to_le_bytes());
    r[40..44].copy_from_slice(&(ssn as u32).to_le_bytes());
    r
}

fn resp(status: u16, failure_frame: u8, rate: u32) -> TxResp {
    parse_tx_resp(&tx_rsp_bytes(status, failure_frame, rate, 0, 0, 0, 1)).unwrap()
}

/// An 802.11g AP: basic 1, 2, 5.5, 11; all OFDM rates.
const G_AP: [u8; 12] = [0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 0x30, 0x48, 0x60, 0x6c];

fn cfg(rssi: i8) -> RateCfg {
    RateCfg { ap_rates: G_AP.to_vec(), max_r500k: 0, valid_ant: ANT_A | ANT_B, fixed_ant: None, band_24: true, rssi: Some(rssi), ht: None }
}

#[test]
fn tx_resp_extra_fields() {
    let b = tx_rsp_bytes(0x0083, 15, legacy_rate(108).unwrap(), 3, 2, 1, 0x77);
    let t = parse_tx_resp(&b).unwrap();
    assert_eq!((t.bt_kill_count, t.failure_rts, t.failure_frame, t.reduced_tpc), (3, 2, 15, 1));
    assert_eq!(t.initial_rate, legacy_rate(108).unwrap());
    assert_eq!(t.ssn, 0x77);
    assert!(!t.ok() && t.air_fail());
    let t = parse_tx_resp(&tx_rsp_bytes(0x0088, 0, 0, 0, 0, 0, 1)).unwrap();
    assert!(!t.ok() && !t.air_fail()); // dest in power save: not the rate's fault
    assert!(parse_tx_resp(&b[..30]).is_none());
}

#[test]
fn rate_codes_round_trip() {
    for &r in LEGACY_R500K.iter() {
        assert_eq!(rate_r500k(legacy_rate_ant(r, ANT_B).unwrap()), Some(r));
        assert_eq!(legacy_rate_ant(r, ANT_B).unwrap() & RATE_MCS_ANT_MSK, RATE_MCS_ANT_B);
    }
    assert_eq!(rate_r500k(1 << 8), None); // HT
    assert_eq!(legacy_rate(108), legacy_rate_ant(108, ANT_A));
}

#[test]
fn erp_element() {
    // SSID "a", rates, ERP 0x03 (non-ERP present + protection), DS.
    let ies = [0u8, 1, b'a', 1, 1, 0x82, 42, 1, 0x03, 3, 1, 6];
    assert_eq!(erp_ie(&ies), Some(0x03));
    assert_eq!(erp_ie(&ies[..6]), None);
    assert_eq!(erp_ie(&[42, 5, 0]), None); // truncated
}

#[test]
fn lq_table_bytes() {
    let mut t = [0u32; 16];
    for (i, e) in t.iter_mut().enumerate() { *e = legacy_rate_ant(LEGACY_R500K[i % 12], ANT_B).unwrap(); }
    let lq = lq_cmd_table(1, &t, ANT_B);
    assert_eq!(lq.len(), 88);
    assert_eq!(lq[0], 1);
    assert_eq!(lq[6], ANT_B);
    assert_eq!(lq[7], 3);
    assert_eq!(u16::from_le_bytes([lq[12], lq[13]]), 4000);
    for i in 0..16 { assert_eq!(u32_at(&lq, 20 + 4 * i), t[i]); }
    assert_eq!(u32_at(&lq, 84), 0);
}

#[test]
fn initial_rate_from_signal() {
    assert_eq!(initial_r500k(-57, true), 108);
    assert_eq!(initial_r500k(-62, true), 96);
    assert_eq!(initial_r500k(-70, true), 48);
    assert_eq!(initial_r500k(-90, true), 2);
    assert_eq!(initial_r500k(-86, false), 18);
    assert_eq!(initial_r500k(-95, false), 12);
    let c = RateCtl::new(&cfg(-57));
    assert_eq!(c.cur_r500k(), 108);
    assert_eq!(c.rates, vec![2, 4, 11, 12, 18, 22, 24, 36, 48, 72, 96, 108]);
    assert_eq!(c.rates[c.floor], 2);
    // No signal known: 24 Mb/s.
    let mut k = cfg(0);
    k.rssi = None;
    assert_eq!(RateCtl::new(&k).cur_r500k(), 48);
}

#[test]
fn retry_chain() {
    let c = RateCtl::new(&cfg(-57));
    let t = c.table();
    let r = |x| legacy_rate_ant(x, ANT_A).unwrap();
    assert_eq!(&t[..4], &[r(108), r(108), r(96), r(72)]);
    // Every lower rate once, down to the lowest basic rate (1 M), which fills.
    assert_eq!(t[12], r(2));
    assert_eq!(t[15], r(2));
    // A g-only AP with basic 6/12/24: the chain stops at 6 M.
    let mut k = cfg(-75);
    k.ap_rates = vec![0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c];
    let c = RateCtl::new(&k);
    assert_eq!(c.cur_r500k(), 48);
    let t = c.table();
    assert_eq!(&t[..5], &[r(48), r(48), r(36), r(24), r(18)]);
    assert!(t[5..].iter().all(|&x| x == r(12)));
    // maxrate caps the list.
    let mut k = cfg(-50);
    k.max_r500k = 48;
    let c = RateCtl::new(&k);
    assert_eq!(c.cur_r500k(), 48);
    assert_eq!(*c.rates.last().unwrap(), 48);
    let lq = c.lq(1);
    assert_eq!(u32_at(&lq, 20), r(48));
}

#[test]
fn action_rules() {
    // Low success: down at once.
    assert_eq!(rate_action(100, 100 * 186, Some(None), None, 177), Action::Down);
    // Nothing known around: probe up.
    assert_eq!(rate_action(900, 900 * 154, Some(None), Some(None), 121), Action::Up);
    // Higher measured better: up.
    assert_eq!(rate_action(600, 600 * 154, Some(Some(1000)), Some(Some(700 * 177)), 121), Action::Up);
    // Both neighbours worse: stay.
    assert_eq!(rate_action(800, 800 * 154, Some(Some(500 * 121)), Some(Some(300 * 177)), 121), Action::Stay);
    // Lower better: down, unless SR is high.
    assert_eq!(rate_action(600, 600 * 186, Some(Some(950 * 177)), None, 177), Action::Down);
    assert_eq!(rate_action(900, 900 * 186, Some(Some(1000 * 177)), None, 177), Action::Stay);
    // Lower unknown, top rate at 60 %: try lower.
    assert_eq!(rate_action(600, 600 * 186, Some(None), None, 177), Action::Down);
    // ... but not when current already beats the lower rate's best case.
    assert_eq!(rate_action(980 - 100, 880 * 186, Some(None), None, 177), Action::Stay);
}

/// Feed `n` frames at the current table start; `ok(i)` decides each frame,
/// `ff` its retries. LQ tables are "sent" whenever due.
fn feed(c: &mut RateCtl, now: &mut u64, n: usize, ok: impl Fn(usize) -> bool, ff: u8) {
    for i in 0..n {
        *now += 25; // 40 frames/s
        let first = c.sent.unwrap()[0];
        let r = if ok(i) { resp(0x0001, ff, first) } else { resp(0x0083, 15, first) };
        c.on_tx(&r, *now);
        if c.due(*now) { c.mark_sent(*now); }
    }
}

#[test]
fn good_link_stays_fast() {
    let mut c = RateCtl::new(&cfg(-50));
    let mut now = 1000;
    c.mark_sent(now);
    feed(&mut c, &mut now, 400, |_| true, 0);
    assert_eq!(c.cur_r500k(), 108);
    assert_eq!(c.downs, 0);
    assert_eq!(c.toggles, 0);
}

#[test]
fn lossy_top_rate_steps_down_and_settles() {
    // 54 M delivers 60 % first try, 48 M 75 %, 36 M and below 97 %.
    let mut c = RateCtl::new(&cfg(-57));
    c.next_probe_ms = u64::MAX; // the antennas are alike here: no comparison
    let mut now = 1000;
    let mut at36 = 0;
    c.mark_sent(now);
    for i in 0..2000 {
        now += 25;
        let t = c.sent.unwrap();
        let rate = rate_r500k(t[0]).unwrap();
        if i >= 1000 && rate == 72 { at36 += 1; }
        let p = match rate { 108 => 60, 96 => 75, _ => 97 };
        // Deterministic "random": frame i succeeds first try if (i*37 % 100) < p,
        // else the second try at the same rate.
        let first_ok = (i * 37 % 100) < p;
        let r = resp(0x0001, if first_ok { 0 } else { 1 }, t[0]);
        c.on_tx(&r, now);
        if c.due(now) { c.mark_sent(now); }
    }
    // Expected throughput: 54 M 0.6*186=112, 48 M 0.75*177=133, 36 M 0.97*154=149.
    // 48 M's window goes stale while 36 M runs, so it is re-probed every few
    // seconds (as rs.c re-explores after its stay-in-column timeout): the
    // controller spends most of the time at 36 M, not all of it.
    assert!(at36 >= 800, "36 M for {} of the last 1000 frames: {}", at36, c.summary(now));
    assert!(c.downs >= 2);
}

#[test]
fn lq_rate_limited_and_mismatch_ignored() {
    let mut c = RateCtl::new(&cfg(-57));
    let mut now = 1000;
    c.mark_sent(now);
    let first = c.sent.unwrap()[0];
    // Three outright failures: window valid, SR 0 -> down, table pending.
    for _ in 0..3 { now += 1; c.on_tx(&resp(0x0083, 15, first), now); }
    assert_eq!(c.cur_r500k(), 96);
    assert!(c.pending);
    assert!(!c.due(now)); // < 100 ms since the last table
    assert!(c.take_lq(1, now).is_none());
    // Responses for frames queued under the old table still arrive: no
    // second step while the new table has not gone out.
    for _ in 0..5 { now += 1; c.on_tx(&resp(0x0083, 15, first), now); }
    assert_eq!(c.cur_r500k(), 96);
    now = 1000 + LQ_MIN_INTERVAL_MS;
    let lq = c.take_lq(1, now).unwrap();
    assert_eq!(u32_at(&lq, 20), legacy_rate(96).unwrap());
    assert!(!c.pending);
    // Now an old-table response is a mismatch and is not counted.
    let before = c.mismatched;
    c.on_tx(&resp(0x0001, 0, first), now + 1);
    assert_eq!(c.mismatched, before + 1);
    // Fixed-rate frames (EAPOL at 12 M) too.
    c.on_tx(&resp(0x0001, 0, legacy_rate(24).unwrap()), now + 2);
    assert_eq!(c.mismatched, before + 2);
}

#[test]
fn retries_charge_the_chain() {
    let mut c = RateCtl::new(&cfg(-57));
    c.mark_sent(1000);
    let first = c.sent.unwrap()[0];
    // Success on the 4th attempt: 54, 54, 48 failed, 36 succeeded.
    c.on_tx(&resp(0x0001, 3, first), 1001);
    let i = |r: u16| c.rates.iter().position(|&x| x == r).unwrap();
    assert_eq!((c.win[i(108)].att, c.win[i(108)].succ), (2, 0));
    assert_eq!((c.win[i(96)].att, c.win[i(96)].succ), (1, 0));
    assert_eq!((c.win[i(72)].att, c.win[i(72)].succ), (1, 1));
    assert_eq!(c.final_rate(&resp(0x0001, 3, first)), legacy_rate(72).unwrap());
    // Postponed / power-save statuses teach nothing.
    let before = c.win.clone();
    c.on_tx(&resp(0x0088, 0, first), 1002);
    c.on_tx(&resp(0x0041, 0, first), 1003);
    assert_eq!(c.win, before);
}

#[test]
fn antenna_toggles_after_a_failure_run() {
    let mut c = RateCtl::new(&cfg(-90)); // starts at 1 M: nowhere lower to go
    let mut now = 1000;
    c.mark_sent(now);
    assert_eq!(c.cur_r500k(), 2);
    assert_eq!(c.ant, ANT_A);
    feed(&mut c, &mut now, FAIL_STREAK_TOGGLE as usize, |_| false, 0);
    assert_eq!(c.toggles, 1);
    assert_eq!(c.ant, ANT_B);
    assert_eq!(c.sent.unwrap()[0] & RATE_MCS_ANT_MSK, RATE_MCS_ANT_B);
    // Not again within TOGGLE_MIN_MS.
    feed(&mut c, &mut now, 8, |_| false, 0);
    assert_eq!(c.toggles, 1);
    // A forced antenna never toggles; an antenna that is not valid is ignored.
    let mut k = cfg(-90);
    k.fixed_ant = Some(ANT_B);
    let mut c = RateCtl::new(&k);
    c.mark_sent(now);
    assert_eq!(c.ant, ANT_B);
    feed(&mut c, &mut now, 40, |_| false, 0);
    assert_eq!(c.toggles, 0);
    let mut k = cfg(-90);
    k.valid_ant = ANT_A;
    k.fixed_ant = Some(ANT_B);
    let mut c = RateCtl::new(&k);
    c.mark_sent(now);
    assert_eq!(c.ant, ANT_A);
    feed(&mut c, &mut now, 40, |_| false, 0);
    assert_eq!(c.toggles, 0);
}

#[test]
fn recovers_upward_after_stale_windows() {
    let mut c = RateCtl::new(&cfg(-57));
    let mut now = 1000;
    c.mark_sent(now);
    // A bad patch: everything above 12 M fails.
    for _ in 0..600 {
        now += 25;
        let t = c.sent.unwrap();
        let ok = rate_r500k(t[0]).unwrap() <= 24;
        c.on_tx(&if ok { resp(0x0001, 0, t[0]) } else { resp(0x0083, 15, t[0]) }, now);
        if c.due(now) { c.mark_sent(now); }
    }
    assert!(c.cur_r500k() <= 36, "{}", c.summary(now));
    // Then the channel clears: it climbs back to 54 M.
    feed(&mut c, &mut now, 3000, |_| true, 0);
    assert_eq!(c.cur_r500k(), 108, "{}", c.summary(now));
}

#[test]
fn counters_split() {
    let mut s = TxStats::default();
    s.on_enqueue(Ok(()));
    s.on_enqueue(Ok(()));
    s.on_enqueue(Ok(()));
    s.on_enqueue(Err(EnqFail::Busy));
    s.on_enqueue(Err(EnqFail::NotReady));
    s.on_enqueue(Err(EnqFail::Capped));
    let r54 = legacy_rate(108).unwrap();
    s.on_resp(&parse_tx_resp(&tx_rsp_bytes(0x0001, 2, r54, 1, 0, 0, 1)).unwrap(), legacy_rate(72).unwrap(), true);
    s.on_resp(&parse_tx_resp(&tx_rsp_bytes(0x0083, 15, r54, 0, 3, 1, 1)).unwrap(), legacy_rate(2).unwrap(), true);
    s.on_resp(&parse_tx_resp(&tx_rsp_bytes(0x008a, 0, r54, 2, 0, 0, 1)).unwrap(), r54, true);
    s.on_resp(&parse_tx_resp(&tx_rsp_bytes(0x0043, 0, r54, 0, 0, 0, 1)).unwrap(), r54, true);
    s.on_resp(&resp(0x0001, 0, mgmt_rate(1)), mgmt_rate(1), false);
    s.on_resp(&resp(0x0083, 15, mgmt_rate(1)), mgmt_rate(1), false);
    assert_eq!((s.enq, s.ok, s.fail_fw, s.busy, s.notready, s.capped), (3, 1, 3, 1, 1, 1));
    assert_eq!(s.failed_total(), 6);
    assert_eq!((s.mgmt_ok, s.mgmt_fail), (1, 1));
    assert_eq!(s.status, [1, 0, 1, 0, 1, 0, 1, 0, 0]);
    assert_eq!(s.tries_ok[2], 1);
    assert_eq!(s.tries_fail[15], 1);
    assert_eq!(s.tries_fail[0], 2);
    assert_eq!(s.rate_ok[9], 1); // 36 M
    assert_eq!(s.rate_fail[0], 1); // 1 M
    assert_eq!(s.rate_fail[11], 2); // 54 M
    assert_eq!((s.bt_kill, s.bt_kill_frames, s.rts_fail, s.tpc_reduced), (3, 2, 3, 1));
    let mark = s;
    s.on_enqueue(Ok(()));
    s.on_resp(&resp(0x0001, 0, r54), r54, true);
    let d = s.delta(&mark);
    assert_eq!((d.enq, d.ok, d.fail_fw, d.busy), (1, 1, 0, 0));
    assert_eq!(d.status[0], 1);
    assert_eq!(d.rate_ok[11], 1);
    let line = d.line();
    assert!(line.starts_with("enq 1 ok 1 fail_fw 0"), "{}", line);
    assert!(line.contains("status 01:1;"), "{}", line);
    assert!(line.contains("rate(ok) 54M:1;"), "{}", line);
    let mut out = String::new();
    s.detail(&mut out);
    assert!(out.contains("tx_status    83:        1"), "{}", out);
}

#[test]
fn status_buckets() {
    assert_eq!(status_bucket(0x0001), 0);
    assert_eq!(status_bucket(0x0002), 1);
    for st in 0x40..=0x4f { assert_eq!(status_bucket(st), 2); }
    assert_eq!(status_bucket(0x0082), 3);
    assert_eq!(status_bucket(0x0183), 4); // upper bits ignored
    assert_eq!(status_bucket(0x0088), 5);
    assert_eq!(status_bucket(0x008a), 6);
    assert_eq!(status_bucket(0x0090), 7);
    assert_eq!(status_bucket(0x0084), 8);
    assert_eq!(rate_bucket(legacy_rate(11).unwrap()), 2);
    assert_eq!(rate_bucket(1 << 8), 12);
}

#[test]
fn bt_coex_wifi_mode() {
    assert_eq!(crate::iwl_cmd::bt_config_wifi_cmd(), vec![3, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn bss_erp_parsed() {
    use crate::wifi_frame::parse_beacon;
    // Beacon: header (24) + fixed (12) + SSID + rates + DS + ERP.
    let mut f = vec![0x80, 0, 0, 0];
    f.extend_from_slice(&[0xff; 6]);
    f.extend_from_slice(&[2, 0, 0, 0, 0, 1]);
    f.extend_from_slice(&[2, 0, 0, 0, 0, 1]);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[0; 8]);
    f.extend_from_slice(&100u16.to_le_bytes());
    f.extend_from_slice(&0x0421u16.to_le_bytes());
    f.extend_from_slice(&[0, 1, b'x', 1, 2, 0x82, 0x0c, 3, 1, 6, 42, 1, 0x02]);
    let b = parse_beacon(&f, 6).unwrap();
    assert_eq!(b.erp, Some(0x02));
    assert_eq!(b.channel, 6);
}

fn ht_cfg(rssi: i8, sgi: bool, mimo: bool) -> RateCfg {
    RateCfg { ht: Some(HtRates { sgi, mimo, ht40: false, stbc: false, ldpc: false, vht: None }), ..cfg(rssi) }
}

#[test]
fn ht_rate_ids_and_tables() {
    assert_eq!(expected_tpt(ht_id(0, false)), 42);
    assert_eq!(expected_tpt(ht_id(7, true)), 210);
    assert_eq!(expected_tpt(ht_id(15, false)), 250);
    assert_eq!(expected_tpt(108), 186);
    assert_eq!(nominal_r500k(ht_id(7, false)), 130);
    assert_eq!(nominal_r500k(ht_id(12, false)), 156);
    assert_eq!(rate_label(ht_id(5, true)), "MCS5s");
    assert_eq!(rate_label(11), "5.5M");
    for id in [ht_id(0, false), ht_id(7, true), ht_id(15, false), 2, 108] {
        assert_eq!(rate_id(rate_code(id, ANT_B)), Some(id));
    }
    assert_eq!(rate_code(ht_id(9, false), ANT_A) & RATE_MCS_ANT_MSK, 3 << 14);
    assert_eq!(initial_mcs(-74), 3);
    assert_eq!(initial_mcs(-60), 7);
    assert_eq!(initial_mcs(-90), 0);
    assert_eq!(rate_bucket(ht_rate(5, true, ANT_A)), 17);
    assert_eq!(rate_bucket(ht_rate(15, false, ANT_AB)), 27);
    assert_eq!(rate_bucket(ht_rate(20, false, ANT_AB)), 28);
    assert_eq!(rate_bucket(legacy_rate(108).unwrap()), 11);
}

#[test]
fn ht_ladder_and_retry_chain() {
    let c = RateCtl::new(&ht_cfg(-74, false, false));
    let mcs = |m| ht_id(m, false);
    assert_eq!(c.rates, vec![2, 4, 11, 12, mcs(0), mcs(1), mcs(2), mcs(3), mcs(4), mcs(5), mcs(6), mcs(7)]);
    assert_eq!(c.floor, 0);
    assert_eq!(c.cur_label(), "MCS3");
    let t = c.table();
    let h = |m| ht_rate(m, false, ANT_A);
    let l = |r| legacy_rate_ant(r, ANT_A).unwrap();
    assert_eq!(&t[..11], &[h(3), h(3), h(2), h(2), h(1), h(1), h(0), l(12), l(11), l(4), l(2)]);
    assert!(t[11..].iter().all(|&x| x == l(2)));
    assert_eq!(RateCtl::mimo_delim(&t), 0);
    let lq = c.lq(1);
    assert_eq!(lq[5], 0);
    assert_eq!(u32_at(&lq, 20), h(3));

    // Short GI.
    let c = RateCtl::new(&ht_cfg(-50, true, false));
    assert_eq!(c.cur_label(), "MCS7s");
    assert_eq!(c.table()[0], ht_rate(7, true, ANT_A));

    // MIMO above MCS 7: MCS 12-15; a table starting there has its SISO part after mimo_delim.
    let mut c = RateCtl::new(&ht_cfg(-50, false, true));
    assert_eq!(&c.rates[12..], &[mcs(12), mcs(13), mcs(14), mcs(15)]);
    c.cur = c.rates.len() - 1;
    let t = c.table();
    let m = |x| ht_rate(x, false, ANT_AB);
    assert_eq!(&t[..8], &[m(15), m(15), m(14), m(14), m(13), m(13), m(12), h(7)]);
    assert_eq!(RateCtl::mimo_delim(&t), 7);
    assert_eq!(c.lq(1)[5], 7);

    // The rate cap applies to HT too: 24 Mb/s keeps MCS 0-2.
    let c = RateCtl::new(&RateCfg { max_r500k: 48, ..ht_cfg(-50, false, true) });
    assert_eq!(c.rates, vec![2, 4, 11, 12, mcs(0), mcs(1), mcs(2)]);
    assert_eq!(c.cur_label(), "MCS2");
}

#[test]
fn ht_rate_control_climbs_on_success_and_falls_on_failure() {
    let mut c = RateCtl::new(&ht_cfg(-74, false, false));
    let mut now = 1000;
    c.mark_sent(now);
    // Everything gets through: the rate climbs to MCS 7.
    for _ in 0..400 {
        now += 2;
        let t = c.sent.unwrap();
        c.on_tx(&resp(TX_STATUS_SUCCESS as u16, 0, t[0]), now);
        if c.due(now) { c.mark_sent(now); }
    }
    assert_eq!(c.cur_label(), "MCS7");
    // Above MCS 4 nothing gets through on the first try: it settles at MCS 4.
    for _ in 0..2000 {
        now += 2;
        let t = c.sent.unwrap();
        let id = rate_id(t[0]).unwrap();
        let r = if is_ht(id) && (id & 0x1f) > 4 { resp(TX_STATUS_SUCCESS as u16, 2, t[0]) } else { resp(TX_STATUS_SUCCESS as u16, 0, t[0]) };
        c.on_tx(&r, now);
        if c.due(now) { c.mark_sent(now); }
    }
    assert_eq!(c.cur_label(), "MCS4");
    assert_eq!(c.mismatched, 0);
}
