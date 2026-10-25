//! TX aggregation (iwl_agg): BA notification / A-MPDU response parsing, the
//! session state machine, and the command encodings it drives.

use crate::iwl_agg::*;
use crate::iwl_assoc as asc;
use crate::iwl_cmd as cmd;
use crate::iwl_rs::{self as rs, HtRates, RateCfg, RateCtl};
use crate::wifi_ht as ht;

fn ba_bytes(sta: u8, tid: u8, flow: u16, ssn: u16, txed: u8, done: u8) -> Vec<u8> {
    let mut v = vec![0x74, 0x83, 0xc2, 0xba, 0x21, 0xdf, 0, 0, sta, tid];
    v.extend_from_slice(&(0x1230u16).to_le_bytes());
    v.extend_from_slice(&0xffff_0000_ffffu64.to_le_bytes());
    v.extend_from_slice(&flow.to_le_bytes());
    v.extend_from_slice(&ssn.to_le_bytes());
    v.extend_from_slice(&[txed, done, 0, 0]);
    v
}

#[test]
fn ba_notif_layout() {
    let n = parse_ba_notif(&ba_bytes(1, 0, 4, 0x37, 12, 11)).unwrap();
    assert_eq!(n.sta_addr, [0x74, 0x83, 0xc2, 0xba, 0x21, 0xdf]);
    assert_eq!((n.sta_id, n.tid, n.seq_ctl), (1, 0, 0x1230));
    assert_eq!(n.bitmap, 0xffff_0000_ffff);
    assert_eq!((n.scd_flow, n.scd_ssn, n.txed, n.txed_2_done), (4, 0x37, 12, 11));
    assert!(parse_ba_notif(&ba_bytes(1, 0, 4, 0, 1, 1)[..27]).is_none());
}

#[test]
fn agg_tx_resp_statuses() {
    // frame_count 3, the status array at 36 (status, sequence) x 3, then the SSN.
    let mut p = vec![0u8; 36 + 12 + 4];
    p[0] = 3;
    for i in 0..3u16 {
        p[36 + 4 * i as usize..38 + 4 * i as usize].copy_from_slice(&(i * 0x10).to_le_bytes());
        p[38 + 4 * i as usize..40 + 4 * i as usize].copy_from_slice(&(0x400 + i).to_le_bytes());
    }
    assert_eq!(agg_statuses(&p), vec![(0, 0x400), (0x10, 0x401), (0x20, 0x402)]);
    assert_eq!(asc::parse_tx_resp(&p).unwrap().frame_count, 3);
}

#[test]
fn session_request_waits_for_link_and_empty_queue() {
    let mut a = TxAgg::new(true);
    assert_eq!(a.poll_request(0, false, true, true, 5), None, "not HT");
    assert_eq!(a.poll_request(0, true, false, true, 5), None, "keys not installed");
    assert_eq!(a.poll_request(0, true, true, false, 5), None, "frames in flight");
    let (d, ssn) = a.poll_request(0, true, true, true, 0x1005).unwrap();
    assert_eq!((d, ssn), (1, 0x005));
    assert!(matches!(a.state, AggState::Requested { dialog: 1, ssn: 5, .. }));
    assert_eq!(a.poll_request(10, true, true, true, 5), None, "one request at a time");
    // Wrong dialog or TID: ignored.
    assert_eq!(a.on_addba_resp(2, 0, 0, 64, 20), None);
    assert_eq!(a.on_addba_resp(1, 0, 3, 64, 20), None);
    assert_eq!(a.on_addba_resp(1, 0, 0, 32, 20), Some(32));
    assert!(a.on());
    assert_eq!(a.lq_frame_limit(), 32);
    assert_eq!(a.poll_request(30, true, true, true, 5), None, "already on");
}

#[test]
fn session_window_clamped_and_disabled_knob() {
    let mut a = TxAgg::new(true);
    a.poll_request(0, true, true, true, 0).unwrap();
    assert_eq!(a.on_addba_resp(1, 0, 0, 0, 1), Some(64));
    assert_eq!(a.lq_frame_limit(), LQ_AGG_FRAME_LIMIT_MAX);
    let mut off = TxAgg::new(false);
    assert_eq!(off.poll_request(0, true, true, true, 0), None);
    assert!(off.line().starts_with("off (iwlwifi.txagg=0)"));
}

#[test]
fn session_timeout_refusal_and_retry_limit() {
    let mut a = TxAgg::new(true);
    a.poll_request(0, true, true, true, 0).unwrap();
    assert_eq!(a.poll_request(ADDBA_TIMEOUT_MS - 1, true, true, true, 0), None);
    assert_eq!(a.poll_request(ADDBA_TIMEOUT_MS, true, true, true, 0), None, "times out, waits");
    assert_eq!(a.st.timeouts, 1);
    assert_eq!(a.state, AggState::Off);
    assert_eq!(a.poll_request(ADDBA_TIMEOUT_MS + 5, true, true, true, 0), None, "retry back-off");
    let t = ADDBA_TIMEOUT_MS + ADDBA_RETRY_MS;
    let (d, _) = a.poll_request(t, true, true, true, 0).unwrap();
    assert_eq!(d, 2);
    // Refused (status 37): back off again; the third try is the last.
    assert_eq!(a.on_addba_resp(2, 37, 0, 0, t + 1), None);
    assert_eq!((a.st.refused, a.st.last_refusal), (1, 37));
    let t2 = t + 1 + ADDBA_RETRY_MS;
    assert!(a.poll_request(t2, true, true, true, 0).is_some());
    a.on_addba_resp(3, 37, 0, 0, t2);
    assert_eq!(a.poll_request(t2 + 10 * ADDBA_RETRY_MS, true, true, true, 0), None, "three tries per association");
    a.reset();
    assert!(a.poll_request(t2 + 10 * ADDBA_RETRY_MS, true, true, true, 0).is_some(), "a new association tries again");
}

#[test]
fn delba_and_fw_refusal_end_the_session() {
    let mut a = TxAgg::new(true);
    a.poll_request(0, true, true, true, 0).unwrap();
    a.on_addba_resp(1, 0, 0, 64, 1);
    assert!(!a.on_delba(5), "other TID");
    assert!(a.on_delba(0));
    assert!(!a.on());
    assert_eq!(a.poll_request(1_000_000, true, true, true, 0), None, "no retry after the AP ended it");
    let mut b = TxAgg::new(true);
    b.poll_request(0, true, true, true, 0).unwrap();
    b.on_addba_resp(1, 0, 0, 64, 1);
    b.fw_refused();
    assert!(!b.on());
    assert_eq!(b.poll_request(1_000_000, true, true, true, 0), None);
}

#[test]
fn stats_line() {
    let mut a = TxAgg::new(true);
    a.poll_request(0, true, true, true, 0).unwrap();
    a.on_addba_resp(1, 0, 0, 64, 1);
    a.on_agg_rsp(10);
    a.on_agg_rsp(5);
    a.on_ba(&parse_ba_notif(&ba_bytes(1, 0, 4, 10, 15, 14)).unwrap(), 10);
    let l = a.line();
    assert!(l.starts_with("on win64; addba 1 accepted 1"), "{}", l);
    assert!(l.contains("ampdu 2 frames 15 avg 7.5 max 10; ba 1 txed 15 acked 14"), "{}", l);
}

#[test]
fn commands() {
    // SCD_QUEUE_CFG: token, sta, tid, queue, enable, aggregate, fifo, window, ssn, reserved.
    assert_eq!(cmd::scd_queue_cfg_cmd_agg(4, true, 1, 0, 1, 0x23, true, 64), vec![0, 1, 0, 4, 1, 1, 1, 64, 0x23, 0, 0, 0]);
    assert_eq!(cmd::scd_queue_cfg_cmd(4, true, 1, 8, 1, 7), vec![0, 1, 8, 4, 1, 0, 1, 64, 7, 0, 0, 0]);
    // ADD_STA: modify, tid_disable_tx, sta id, modify_mask QUEUES | TID_DISABLE_TX, queue mask.
    let a = cmd::add_sta_tx_agg(1, 0, 0x30, 0xfffe);
    assert_eq!(a.len(), 48);
    assert_eq!(a[0], 1);
    assert_eq!(u16::from_le_bytes([a[2], a[3]]), 0xfffe);
    assert_eq!(a[16], 1);
    assert_eq!(a[17], (1 << 7) | (1 << 1));
    assert_eq!(u32::from_le_bytes([a[40], a[41], a[42], a[43]]), 0x30);
}

#[test]
fn bar_tx_cmd() {
    let f = ht::build_bar(&[1; 6], &[2; 6], 0, 0x123);
    assert_eq!(f.len(), 20);
    let p = asc::TxParams { sta_id: 1, rate: Some(asc::mgmt_rate(6)), tid: 0, retries: 15, pm_frame_timeout: 0, ccmp_key: None };
    let t = asc::tx_cmd(&p, f.len(), &f[..16]);
    let flags = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
    assert_eq!(flags & asc::TX_CMD_FLG_BAR, asc::TX_CMD_FLG_BAR);
    assert_eq!(flags & asc::TX_CMD_FLG_ACK, asc::TX_CMD_FLG_ACK);
    assert_eq!(flags & asc::TX_CMD_FLG_SEQ_CTL, 0, "the BAR has no sequence number");
    assert_eq!(t.len(), asc::TX_CMD_LEN + 16);
    assert_eq!(&t[asc::TX_CMD_LEN..], &f[..16]);
}

fn ht_ctl() -> RateCtl {
    RateCtl::new(&RateCfg { ap_rates: vec![0x82, 0x84, 0x8b, 0x96, 12, 18, 24, 36, 48, 72, 96, 108], max_r500k: 0,
                            valid_ant: asc::ANT_AB, fixed_ant: None, band_24: false, rssi: Some(-76),
                            ht: Some(HtRates { sgi: false, mimo: false, ht40: false, stbc: false, ldpc: false }) })
}

#[test]
fn rate_control_counts_ampdus_and_climbs() {
    let mut c = ht_ctl();
    c.mark_sent(0);
    let start = c.cur;
    c.set_agg(true, 63);
    assert!(c.pending);
    let lq = c.lq(1);
    assert_eq!(lq[15], 63, "agg_frame_cnt_limit");
    c.mark_sent(0);
    let mut now = 200;
    for _ in 0..40 {
        let r = c.table()[0];
        c.on_ampdu(r, 16, 16, now);
        if c.due(now) { c.mark_sent(now); }
        now += 120;
    }
    assert!(c.cur > start, "an all-acked link climbs ({} -> {})", start, c.cur);
    assert!(c.ampdus > 0);
    // An A-MPDU at another rate than the table's: not counted.
    let before = c.mismatched;
    c.on_ampdu(rs::rate_code(rs::ht_id(0, false), asc::ANT_A), 8, 8, now);
    assert_eq!(c.mismatched, before + 1);
}

#[test]
fn rate_control_steps_down_on_lost_ampdus() {
    let mut c = ht_ctl();
    c.set_agg(true, 63);
    c.mark_sent(0);
    let start = c.cur;
    let mut now = 200;
    for _ in 0..20 {
        let r = c.table()[0];
        c.on_ampdu(r, 16, 1, now);
        if c.due(now) { c.mark_sent(now); }
        now += 120;
    }
    assert!(c.cur < start, "a mostly lost link steps down ({} -> {})", start, c.cur);
    assert!(rs::expected_tpt_agg(rs::ht_id(7, false)) > rs::expected_tpt(rs::ht_id(7, false)));
}

#[test]
fn ampdu_success_ratio_is_the_window_ratio() {
    // 24 subframes per A-MPDU, 18 acked: the rate delivers 75 %. The failed
    // subframes are recorded last; an EWMA over attempts read that as ~45 %
    // and stepped the rate down on a healthy aggregated link.
    let mut c = ht_ctl();
    c.mark_sent(0);
    c.set_agg(true, 63);
    c.mark_sent(0);
    let idx = c.cur;
    let r = c.table()[0];
    for k in 0..3 { c.on_ampdu(r, 24, 18, 200 + k * 50); }
    let w = c.win[idx];
    assert!((700..=800).contains(&w.sr), "sr {} permille over {} attempts", w.sr, w.att);
    assert_eq!(w.att, rs::RATE_WINDOW, "the window holds the last 62 attempts");
    // A fully acked burst moves the ratio up by its share of the window only.
    c.on_ampdu(r, 24, 24, 400);
    assert!(c.win[idx].sr < 950, "sr {}", c.win[idx].sr);
}

#[test]
fn sn_of_ring_index() {
    assert_eq!(sn_of_index(141, 141 & 0xff), 141);
    assert_eq!(sn_of_index(300, 40), 296);
    assert_eq!(sn_of_index(300, 50), 50);
    assert_eq!(sn_of_index(5, 250), 4090);
}

/// Guard (L12 #63): a router that holds its window at a hole until a BAR
/// must get data again within 1 s after silently given-up subframes.
#[test]
fn ba_hole_resumes_within_1s() {
    let r = sim_bar_hole(3, 300_000, true, 3_000);
    assert!(r.bars > 0, "{:?}", r);
    assert!(r.resume_ms.map_or(false, |t| t < 1000), "{:?}", r);
    assert!(!r.teardown, "{:?}", r);
}

#[test]
fn bar_rate_limited_and_single_fail_ssn() {
    let mut a = TxAgg::new(true);
    a.poll_request(0, true, true, true, 0).unwrap();
    a.on_addba_resp(1, 0, 0, 64, 1);
    a.on_single_fail(4095);
    assert_eq!(a.next_bar(10), Some(0));
    assert_eq!(a.next_bar(11), None); // status pending
    assert!(!a.on_bar_status(true, 12));
    a.on_single_fail(7);
    assert_eq!(a.next_bar(20), None); // < BAR_MIN_INTERVAL_MS
    assert_eq!(a.next_bar(60), Some(8));
}

#[test]
fn failing_bars_tear_down_the_session() {
    let r = sim_bar_hole(3, 300_000, false, 3_000);
    assert!(r.teardown && r.bars == BAR_MAX_FAILS as u64, "{:?}", r);
    let mut a = TxAgg::new(true);
    a.poll_request(0, true, true, true, 0).unwrap();
    a.on_addba_resp(1, 0, 0, 64, 1);
    for i in 0..BAR_MAX_FAILS {
        a.on_single_fail(10);
        let t = 100 * (i as u64 + 1);
        assert!(a.next_bar(t).is_some());
        assert_eq!(a.on_bar_status(false, t), i + 1 == BAR_MAX_FAILS);
    }
    assert!(!a.on() && a.st.bar_teardowns == 1 && a.st.bars_failed == 3);
    assert!(a.line().contains("bar_fail 3 bar_teardown 1"), "{}", a.line());
}
