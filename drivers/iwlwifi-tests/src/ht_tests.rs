//! Tests for the WMM / HT / Block Ack module (drivers/src/wifi_ht.rs).

use crate::wifi_frame as wf;
use crate::wifi_ht::*;

const AP: [u8; 6] = [0x74, 0x83, 0xc2, 0xba, 0x21, 0xdf];
const STA: [u8; 6] = [0x00, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];

/// hostapd's default WMM Parameter element (wmm_ac_* defaults).
fn hostapd_wmm() -> Vec<u8> {
    vec![0xdd, 0x18, 0x00, 0x50, 0xf2, 0x02, 0x01, 0x01, 0x81, 0x00,
         0x03, 0xa4, 0x00, 0x00, // BE: AIFSN 3, ECW 4/10
         0x27, 0xa4, 0x00, 0x00, // BK: ACI 1, AIFSN 7
         0x42, 0x43, 0x5e, 0x00, // VI: ACI 2, AIFSN 2, ECW 3/4, TXOP 94
         0x62, 0x32, 0x2f, 0x00] // VO: ACI 3, AIFSN 2, ECW 2/3, TXOP 47
}

fn ht_cap_2x2() -> Vec<u8> {
    let mut v = vec![45, 26, 0xef, 0x09, 0x17, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    v.resize(28, 0);
    v
}

fn ht_op(primary: u8, op_mode: u16) -> Vec<u8> {
    let mut v = vec![61, 22, primary, 0x00];
    v.extend_from_slice(&op_mode.to_le_bytes());
    v.resize(24, 0);
    v
}

#[test]
fn wmm_info_ie_bytes() {
    assert_eq!(wmm_info_ie(0), [0xdd, 7, 0x00, 0x50, 0xf2, 0x02, 0x00, 0x01, 0x00]);
}

#[test]
fn wmm_param_parsed_and_mapped_for_the_firmware() {
    let ie = hostapd_wmm();
    let w = find_wmm(&ie).expect("wmm");
    let p = match w { Wmm::Param(p) => p, _ => panic!("not a parameter element") };
    assert_eq!(p.qos_info, 0x81);
    assert_eq!(p.set_count(), 1);
    assert_eq!(p.ac[ACI_BE], AcParams { aifsn: 3, acm: false, cw_min: 15, cw_max: 1023, txop: 0 });
    assert_eq!(p.ac[ACI_BK].aifsn, 7);
    assert_eq!(p.ac[ACI_VI], AcParams { aifsn: 2, acm: false, cw_min: 7, cw_max: 15, txop: 94 });
    assert_eq!(p.ac[ACI_VO], AcParams { aifsn: 2, acm: false, cw_min: 3, cw_max: 7, txop: 47 });
    // Firmware order BK, BE, VI, VO; TXOP in microseconds; FIFO per AC.
    let f = fw_edca(&p);
    assert_eq!(f[0], (15, 1023, 7, 0, 0));
    assert_eq!(f[1], (15, 1023, 3, 1, 0));
    assert_eq!(f[2], (7, 15, 2, 2, 94 * 32));
    assert_eq!(f[3], (3, 7, 2, 3, 47 * 32));
    // The defaults are the same table.
    assert_eq!(WmmParams::defaults().ac, p.ac);
}

#[test]
fn wmm_bad_records_keep_defaults_and_info_element_implies_defaults() {
    let mut ie = hostapd_wmm();
    ie[10] = 0x01; // BE AIFSN 1: invalid for a station
    ie[15] = 0x4a; // BK ECWmin 10 > ECWmax 4
    let p = find_wmm(&ie).unwrap().params();
    assert_eq!(p.ac[ACI_BE], WmmParams::defaults().ac[ACI_BE]);
    assert_eq!(p.ac[ACI_BK], WmmParams::defaults().ac[ACI_BK]);
    let info = [0xdd, 7, 0x00, 0x50, 0xf2, 0x02, 0x00, 0x01, 0x05];
    assert_eq!(find_wmm(&info), Some(Wmm::Info(5)));
    assert_eq!(find_wmm(&info).unwrap().params().ac, WmmParams::defaults().ac);
    // WPA1 vendor element is not WMM; version 2 is not understood.
    assert_eq!(find_wmm(&[0xdd, 6, 0x00, 0x50, 0xf2, 0x01, 0x01, 0x00]), None);
    let mut v2 = hostapd_wmm();
    v2[7] = 2;
    assert_eq!(find_wmm(&v2), None);
    // Truncated.
    assert_eq!(find_wmm(&hostapd_wmm()[..20]), None);
}

#[test]
fn tid_to_access_category() {
    assert_eq!(tid_to_aci(0), ACI_BE);
    assert_eq!(tid_to_aci(1), ACI_BK);
    assert_eq!(tid_to_aci(2), ACI_BK);
    assert_eq!(tid_to_aci(3), ACI_BE);
    assert_eq!(tid_to_aci(5), ACI_VI);
    assert_eq!(tid_to_aci(6), ACI_VO);
    assert_eq!(tid_to_aci(7), ACI_VO);
}

#[test]
fn our_ht_capabilities_for_the_8265() {
    let c = iwl8265_ht_cfg(3, 3);
    let ie = ht_cap_ie(&c);
    assert_eq!(ie[0], 45);
    assert_eq!(ie[1], 26);
    // LDPC | SM PS disabled | SGI20 | TX STBC | RX STBC 1 stream; no 40 MHz.
    assert_eq!(u16::from_le_bytes([ie[2], ie[3]]), 0x01ad);
    assert_eq!(ie[4], 0x17); // 64K, 4 us density
    assert_eq!(&ie[5..15], &[0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(u16::from_le_bytes([ie[15], ie[16]]), 144);
    assert_eq!(ie[17], 0x01);
    assert!(ie[18..].iter().all(|&b| b == 0));
    let p = parse_ht_cap(&ie[2..]).unwrap();
    assert_eq!(p.rx_streams(), 2);
    assert_eq!(p.sm_ps(), SM_PS_DISABLED);
    assert!(p.sgi20() && p.ldpc() && p.rx_stbc());
    assert_eq!((p.ampdu_factor, p.ampdu_density), (3, 5));

    // One receive chain, SM PS static, no SGI: MCS 0-7, 72 Mb/s; one TX chain: no TX STBC.
    let c1 = HtCfg { rx_streams: 1, tx_streams: 1, ldpc: false, stbc: true, sgi20: false, sm_ps: SM_PS_STATIC, ht40: false };
    let ie = ht_cap_ie(&c1);
    assert_eq!(u16::from_le_bytes([ie[2], ie[3]]), 0x0100);
    assert_eq!(&ie[5..7], &[0xff, 0]);
    assert_eq!(u16::from_le_bytes([ie[15], ie[16]]), 72);
    // TX and RX streams differ: TX_RX_DIFF with one TX stream.
    let c2 = HtCfg { rx_streams: 2, tx_streams: 1, ..c1 };
    assert_eq!(ht_cap_ie(&c2)[17], 0x03);
}

#[test]
fn bss_elements_parsed_together() {
    let mut ies = Vec::new();
    ies.extend_from_slice(&[0, 3, b'a', b'b', b'c']);
    ies.extend_from_slice(&ht_cap_2x2());
    ies.extend_from_slice(&ht_op(6, 0x0003));
    ies.extend_from_slice(&hostapd_wmm());
    let b = parse_bss_ies(&ies);
    let cap = b.ht_cap.unwrap();
    assert_eq!(cap.cap, 0x09ef);
    assert_eq!(cap.rx_streams(), 2);
    let op = b.ht_op.unwrap();
    assert_eq!(op.primary, 6);
    assert_eq!(op.protection(), HT_PROT_NONHT_MIXED);
    assert!(op.needs_ht_prot_20());
    assert!(!parse_ht_op(&ht_op(6, 0x0002)[2..]).unwrap().needs_ht_prot_20());
    assert!(parse_ht_op(&ht_op(6, 0x0001)[2..]).unwrap().needs_ht_prot_20());
    assert!(matches!(b.wmm, Some(Wmm::Param(_))));
    // No HT, no WMM.
    let b = parse_bss_ies(&[0, 0]);
    assert_eq!(b, BssHt::default());
    // A truncated HT element is ignored.
    let b = parse_bss_ies(&[45, 4, 1, 2, 3, 4]);
    assert_eq!(b.ht_cap, None);
}

#[test]
fn ba_parameter_set_bits() {
    let p = BaParams::from_u16(0x1003);
    assert_eq!(p, BaParams { amsdu: true, immediate: true, tid: 0, buf_size: 64 });
    assert_eq!(p.to_u16(), 0x1003);
    let q = BaParams { amsdu: false, immediate: true, tid: 6, buf_size: 32 };
    assert_eq!(q.to_u16(), (32 << 6) | (6 << 2) | 2);
    assert_eq!(BaParams::from_u16(q.to_u16()), q);
}

#[test]
fn addba_request_from_an_ap_parses() {
    // An ADDBA request as an AP sends it: dialog 1, A-MSDU + immediate,
    // TID 0, 64 buffers, no timeout, SSN 1.
    let mut f = vec![0xd0, 0x00, 0x3a, 0x01];
    f.extend_from_slice(&STA);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0x40, 0x02]);
    f.extend_from_slice(&[0x03, 0x00, 0x01, 0x03, 0x10, 0x00, 0x00, 0x10, 0x00]);
    match parse_back_action(&f) {
        Some(BackAction::AddbaReq { dialog, params, timeout, ssn }) => {
            assert_eq!(dialog, 1);
            assert_eq!(params, BaParams { amsdu: true, immediate: true, tid: 0, buf_size: 64 });
            assert_eq!(timeout, 0);
            assert_eq!(ssn, 1);
        }
        other => panic!("{:?}", other),
    }
    // Our builder writes the same body.
    let b = build_addba_req(&STA, &AP, &AP, 1, BaParams::from_u16(0x1003), 0, 1);
    assert_eq!(&b[24..], &f[24..]);
    assert_eq!(&b[..2], &[0xd0, 0x00]);
    // Not Block Ack: none. Truncated: none.
    let mut g = f.clone();
    g[24] = 4;
    assert_eq!(parse_back_action(&g), None);
    assert_eq!(parse_back_action(&f[..30]), None);
}

#[test]
fn addba_response_and_delba_round_trip() {
    let p = BaParams { amsdu: false, immediate: true, tid: 5, buf_size: 64 };
    let r = build_addba_resp(&AP, &STA, &AP, 9, STATUS_SUCCESS, p, 5000);
    assert_eq!(wf::addr1(&r), Some(AP));
    assert_eq!(wf::addr2(&r), Some(STA));
    assert_eq!(&r[24..], &[3, 1, 9, 0, 0, 0x16, 0x10, 0x88, 0x13]);
    assert_eq!(parse_back_action(&r), Some(BackAction::AddbaResp { dialog: 9, status: 0, params: p, timeout: 5000 }));
    let d = build_delba(&STA, &AP, &AP, 5, true, REASON_END_BA);
    assert_eq!(&d[24..], &[3, 2, 0x00, 0x58, 37, 0]);
    assert_eq!(parse_back_action(&d), Some(BackAction::Delba { tid: 5, initiator: true, reason: 37 }));
    let d = build_delba(&AP, &STA, &AP, 0, false, 39);
    assert_eq!(parse_back_action(&d), Some(BackAction::Delba { tid: 0, initiator: false, reason: 39 }));
}

#[test]
fn addba_answer_rules() {
    let req = BaParams { amsdu: true, immediate: true, tid: 0, buf_size: 64 };
    assert_eq!(answer_addba(&req, true, 64, true), (STATUS_SUCCESS, req));
    assert_eq!(answer_addba(&req, true, 64, false).1.amsdu, false);
    // Window 0 = the maximum; larger requests are clamped.
    assert_eq!(answer_addba(&BaParams { buf_size: 0, ..req }, true, 64, true).1.buf_size, 64);
    assert_eq!(answer_addba(&BaParams { buf_size: 256, ..req }, true, 64, true).1.buf_size, 64);
    assert_eq!(answer_addba(&BaParams { buf_size: 16, ..req }, true, 64, true).1.buf_size, 16);
    // Disabled, delayed policy, TID 8+.
    assert_eq!(answer_addba(&req, false, 64, true).0, STATUS_REQUEST_DECLINED);
    assert_eq!(answer_addba(&BaParams { immediate: false, ..req }, true, 64, true).0, STATUS_INVALID_QOS_PARAM);
    assert_eq!(answer_addba(&BaParams { tid: 9, ..req }, true, 64, true).0, STATUS_INVALID_QOS_PARAM);
}

#[test]
fn block_ack_request_round_trip() {
    let b = build_bar(&STA, &AP, 3, 4095);
    assert_eq!(b.len(), 20);
    assert_eq!(&b[..2], &[0x84, 0x00]);
    assert_eq!(parse_bar(&b), Some((AP, 3, 4095)));
    assert_eq!(ctl_ra(&b), Some(STA));
    let mut m = b.clone();
    m[16] |= 2; // multi-TID
    assert_eq!(parse_bar(&m), None);
    assert_eq!(parse_bar(&b[..18]), None);
}

fn eth(n: u8) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&STA);
    e.extend_from_slice(&[0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    e.extend_from_slice(&[0x08, 0x00]);
    e.extend_from_slice(&[n; 40]);
    e
}

#[test]
fn downlink_qos_frame_converts_back() {
    let f = build_qos_data_from_ap(&eth(7), &AP, 5, 300, true).unwrap();
    assert_eq!(wf::hdr_len(wf::frame_control(&f).unwrap()), 26);
    assert_eq!(qos_tid_seq(&f), Some((5, 300)));
    let mut out = [0u8; 200];
    let n = wf::data_to_ethernet(&f, 0, 0, &mut out).unwrap();
    assert_eq!(&out[..n], &eth(7)[..]);
    let mut g = wf::RxGuard::new(AP, STA);
    assert_eq!(g.check(&f, false), Ok(()));
    // Group addressed and no-ack frames are not reordered.
    let mut b = eth(1);
    b[0..6].copy_from_slice(&[0xff; 6]);
    assert_eq!(qos_tid_seq(&build_qos_data_from_ap(&b, &AP, 0, 1, false).unwrap()), None);
    let mut na = build_qos_data_from_ap(&eth(1), &AP, 0, 1, false).unwrap();
    na[24] |= 0x20;
    assert_eq!(qos_tid_seq(&na), None);
}

#[test]
fn guard_split_matches_the_single_check() {
    let f = build_qos_data_from_ap(&eth(1), &AP, 0, 10, false).unwrap();
    let mut a = wf::RxGuard::new(AP, STA);
    let mut b = wf::RxGuard::new(AP, STA);
    assert_eq!(a.check(&f, false), Ok(()));
    assert_eq!(b.check_addr(&f), Ok(()));
    assert_eq!(b.check_seq(&f, false), Ok(()));
    let mut r = f.clone();
    r[1] |= 0x08; // retry of the same frame
    assert_eq!(a.check(&r, false), Err(wf::RxReject::Duplicate));
    assert_eq!(b.check_addr(&r), Ok(()));
    assert_eq!(b.check_seq(&r, false), Err(wf::RxReject::Duplicate));
    // A frame for another station fails the address half.
    let mut o = eth(1);
    o[0] = 0x02;
    let o = build_qos_data_from_ap(&o, &AP, 0, 11, false).unwrap();
    assert_eq!(b.check_addr(&o), Err(wf::RxReject::Foreign));
}

fn drain<T>(r: &mut Reorder<T>, sn: u16, v: T, now: u64) -> (Fate, Vec<T>) {
    let mut out = Vec::new();
    let f = r.rx(sn, v, now, &mut out);
    (f, out)
}

#[test]
fn reorder_in_order_frames_pass_directly() {
    let mut r: Reorder<u16> = Reorder::new(100, 64);
    for sn in 100..200 {
        assert_eq!(drain(&mut r, sn, sn, 0), (Fate::Direct, vec![sn]));
    }
    assert_eq!(r.head(), 200);
    assert_eq!(r.stats.direct, 100);
}

#[test]
fn reorder_hole_filled_releases_in_order() {
    let mut r: Reorder<u16> = Reorder::new(0, 64);
    assert_eq!(drain(&mut r, 0, 0, 0).1, vec![0]);
    // 1 is lost in the first A-MPDU; 2..5 wait.
    for sn in 2..6 { assert_eq!(drain(&mut r, sn, sn, 1), (Fate::Buffered, vec![])); }
    assert_eq!(r.stored(), 4);
    // The retransmission of 1 releases 1..5.
    assert_eq!(drain(&mut r, 1, 1, 2), (Fate::Buffered, vec![1, 2, 3, 4, 5]));
    assert_eq!(r.stored(), 0);
    assert_eq!(r.head(), 6);
    // A late duplicate of 3: old. A duplicate of a waiting frame: dup.
    assert_eq!(drain(&mut r, 3, 3, 3).0, Fate::Old);
    assert_eq!(drain(&mut r, 8, 8, 3).0, Fate::Buffered);
    assert_eq!(drain(&mut r, 8, 80, 3).0, Fate::Dup);
    assert_eq!(r.stats.old, 1);
    assert_eq!(r.stats.dup, 1);
}

#[test]
fn reorder_timeout_gives_up_on_a_hole() {
    let mut r: Reorder<u16> = Reorder::new(10, 64);
    for sn in 12..15 { drain(&mut r, sn, sn, 1000); }
    let mut out = Vec::new();
    r.release(1050, &mut out);
    assert!(out.is_empty());
    r.release(1000 + REORDER_TIMEOUT_MS, &mut out);
    assert!(out.is_empty(), "released at exactly the timeout");
    r.release(1001 + REORDER_TIMEOUT_MS, &mut out);
    assert_eq!(out, vec![12, 13, 14]);
    assert_eq!(r.head(), 15);
    assert_eq!(r.stats.timeouts, 1);
    assert_eq!(r.stats.skipped, 2); // 10 and 11
    // 11 arrives late: old.
    assert_eq!(drain(&mut r, 11, 11, 1200).0, Fate::Old);
}

#[test]
fn reorder_timeout_stops_at_a_later_fresh_hole() {
    let mut r: Reorder<u16> = Reorder::new(0, 64);
    drain(&mut r, 1, 1, 0); // hole at 0, old
    // Hole at 2, fresh: storing 3 releases 1 (timed out), not 3.
    assert_eq!(drain(&mut r, 3, 3, 150), (Fate::Buffered, vec![1]));
    assert_eq!(r.head(), 2);
    let mut out = Vec::new();
    r.release(160, &mut out);
    assert!(out.is_empty());
    assert_eq!(r.stored(), 1);
    out.clear();
    r.release(251, &mut out);
    assert_eq!(out, vec![3]);
    assert_eq!(r.head(), 4);
}

#[test]
fn reorder_frame_beyond_the_window_moves_it() {
    let mut r: Reorder<u16> = Reorder::new(0, 8);
    drain(&mut r, 2, 2, 0);
    drain(&mut r, 3, 3, 0);
    // 10 is beyond 0..8: the head moves to 3 (10 - 8 + 1), releasing 2;
    // 3 then sits at the head and goes too.
    let (f, out) = drain(&mut r, 10, 10, 0);
    assert_eq!(f, Fate::Buffered);
    assert_eq!(out, vec![2, 3]);
    assert_eq!(r.head(), 4);
    assert_eq!(r.stored(), 1);
}

#[test]
fn reorder_bar_releases_up_to_its_ssn() {
    let mut r: Reorder<u16> = Reorder::new(0, 64);
    for sn in [1u16, 2, 5, 6] { drain(&mut r, sn, sn, 0); }
    let mut out = Vec::new();
    r.bar(4, 0, &mut out);
    assert_eq!(out, vec![1, 2]);
    assert_eq!(r.head(), 4);
    out.clear();
    r.bar(5, 0, &mut out);
    assert_eq!(out, vec![5, 6]);
    assert_eq!(r.head(), 7);
    assert_eq!(r.stats.bars, 2);
}

#[test]
fn reorder_wraps_at_4096() {
    let mut r: Reorder<u16> = Reorder::new(4094, 64);
    assert_eq!(drain(&mut r, 4094, 1, 0).0, Fate::Direct);
    assert_eq!(drain(&mut r, 0, 3, 0).0, Fate::Buffered);
    assert_eq!(drain(&mut r, 1, 4, 0).0, Fate::Buffered);
    assert_eq!(drain(&mut r, 4095, 2, 0).1, vec![2, 3, 4]);
    assert_eq!(r.head(), 2);
    assert_eq!(drain(&mut r, 4090, 9, 0).0, Fate::Old);
    assert!(sn_less(4095, 0));
    assert!(!sn_less(0, 4095));
    assert_eq!(sn_add(4095, 2), 1);
    assert_eq!(sn_sub(1, 2), 4095);
}

#[test]
fn reorder_frames_before_the_session_start_pass() {
    let mut r: Reorder<u16> = Reorder::new(50, 64);
    // Before the first in-window frame: earlier ones go through unordered.
    assert_eq!(drain(&mut r, 48, 48, 0), (Fate::PreSession, vec![48]));
    assert_eq!(drain(&mut r, 50, 50, 0), (Fate::Direct, vec![50]));
    assert_eq!(drain(&mut r, 49, 49, 0).0, Fate::Old);
}

#[test]
fn rx_agg_sessions_flush_on_stop_and_restart() {
    let mut a: RxAgg<u16> = RxAgg::default();
    let mut out = Vec::new();
    a.start(0, 0, 64, &mut out);
    assert!(a.has(0) && !a.has(1));
    let r = a.tids[0].as_mut().unwrap();
    r.rx(2, 2, 0, &mut out);
    r.rx(3, 3, 0, &mut out);
    assert!(out.is_empty());
    assert_eq!(a.stored(), 2);
    // A new ADDBA for the same TID flushes the old session in order.
    a.start(0, 100, 32, &mut out);
    assert_eq!(out, vec![2, 3]);
    assert_eq!(a.sessions, 2);
    assert_eq!(a.tids[0].as_ref().unwrap().head(), 100);
    out.clear();
    a.tids[0].as_mut().unwrap().rx(102, 102, 0, &mut out);
    a.tick(50, &mut out);
    assert!(out.is_empty());
    a.tick(101, &mut out);
    assert_eq!(out, vec![102]);
    a.stop_all(&mut out);
    assert_eq!(a.active(), 0);
    let s = a.stats();
    assert_eq!(s.buffered, 3);
    assert_eq!(s.timeouts, 1);
}

#[test]
fn reorder_random_permutations_deliver_every_frame_once_in_order() {
    // A-MPDU-like delivery: frames in bursts of 16, a fraction held back and
    // retransmitted in the next burst, some duplicated; nothing is lost, so
    // the output is exactly 0..N in order.
    let mut seed = 0x1234_5678u32;
    let mut rnd = || { seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5; seed };
    let n = 5000u32;
    let mut r: Reorder<u32> = Reorder::new(0, 64);
    let mut out = Vec::new();
    let mut held: Vec<u32> = Vec::new();
    let mut next = 0u32;
    let mut now = 0u64;
    while next < n || !held.is_empty() {
        let mut burst: Vec<u32> = held.drain(..).collect();
        for _ in 0..16 {
            if next >= n { break; }
            if rnd() % 10 == 0 { held.push(next); } else { burst.push(next); }
            if rnd() % 50 == 0 { burst.push(next); }
            next += 1;
        }
        for x in burst { r.rx((x % 4096) as u16, x, now, &mut out); }
        now += 1;
    }
    r.release(now + 1000, &mut out);
    assert_eq!(out.len(), n as usize);
    assert!(out.windows(2).all(|w| w[1] == w[0] + 1));
}

#[test]
fn reorder_fast_path_agrees_with_rx() {
    // Drive two buffers with the same random sequence: one through `fast`
    // first (falling back to `rx`), one through `rx` only. Same output.
    let mut seed = 0x0bad_cafeu32;
    let mut rnd = || { seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5; seed };
    let mut a: Reorder<u16> = Reorder::new(4000, 32);
    let mut b: Reorder<u16> = Reorder::new(4000, 32);
    let (mut oa, mut ob) = (Vec::new(), Vec::new());
    let mut base = 3990u16;
    for i in 0..20000u64 {
        let sn = sn_add(base, (rnd() % 40) as u16);
        if rnd() % 3 == 0 { base = sn_add(base, 1); }
        let now = i / 4;
        match a.fast(sn) {
            Some(Fate::Direct) | Some(Fate::PreSession) => oa.push(sn),
            Some(_) => {}
            None => { a.rx(sn, sn, now, &mut oa); }
        }
        b.rx(sn, sn, now, &mut ob);
        if i % 50 == 0 { a.release(now, &mut oa); b.release(now, &mut ob); }
    }
    assert_eq!(oa, ob);
    assert_eq!(a.stats, b.stats);
}

#[test]
fn reorder_odd_window_across_the_wrap() {
    // A 48-frame window (not a divisor of 4096) through several wraps of
    // the sequence space: holes closed one burst later, all in order.
    let mut r: Reorder<u32> = Reorder::new(4000, 48);
    let mut out = Vec::new();
    let mut held = Vec::new();
    for x in 0..20000u32 {
        let sn = ((4000 + x) % 4096) as u16;
        if x % 7 == 3 { held.push((sn, x)); continue; }
        r.rx(sn, x, 0, &mut out);
        if x % 16 == 15 { for (s, v) in held.drain(..) { r.rx(s, v, 0, &mut out); } }
    }
    for (s, v) in held.drain(..) { r.rx(s, v, 0, &mut out); }
    r.release(1000, &mut out);
    assert_eq!(out.len(), 20000);
    assert!(out.windows(2).all(|w| w[1] == w[0] + 1));
}
