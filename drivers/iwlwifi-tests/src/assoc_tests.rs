use crate::iwl_assoc::*;
use crate::iwl_cmd::{FW_CTXT_ACTION_ADD, FW_CTXT_ACTION_MODIFY, FW_CTXT_ACTION_REMOVE};

fn u32_at(v: &[u8], o: usize) -> u32 { u32::from_le_bytes([v[o], v[o + 1], v[o + 2], v[o + 3]]) }

#[test]
fn mac_context_layout() {
    let mut s = MacState::unassoc([2, 0, 0, 0, 0, 1]);
    let c = mac_ctxt_cmd(0, FW_CTXT_ACTION_ADD, &s);
    assert_eq!(c.len(), 148);
    assert_eq!(u32_at(&c, 4), FW_CTXT_ACTION_ADD);
    assert_eq!(u32_at(&c, 8), FW_MAC_TYPE_BSS_STA);
    assert_eq!(&c[16..22], &[2, 0, 0, 0, 0, 1]);
    assert_eq!(&c[24..30], &[0xff; 6]);
    assert_eq!(u32_at(&c, 52), (1 << 2) | (1 << 6)); // accept group + beacons
    assert_eq!(u32_at(&c, 100), 0); // not associated
    // BE (index 1) EDCA: cw 15/1023, aifs 3, FIFO BE.
    assert_eq!(&c[68..76], &[15, 0, 0xff, 3, 3, 1 << 1, 0, 0]);
    s.assoc = true;
    s.aid = 5;
    s.beacon_int = 102;
    s.dtim_period = 3;
    s.dtim_tsf = 0x1122_3344_5566;
    s.dtim_time = 777;
    let c = mac_ctxt_cmd(0, FW_CTXT_ACTION_MODIFY, &s);
    assert_eq!(u32_at(&c, 52), 1 << 2);
    assert_eq!(u32_at(&c, 100), 1);
    assert_eq!(u32_at(&c, 104), 777);
    assert_eq!(u32_at(&c, 108), 0x3344_5566);
    assert_eq!(u32_at(&c, 116), 102); // bi
    assert_eq!(u32_at(&c, 124), 306); // dtim interval
    assert_eq!(u32_at(&c, 136), 5); // aid
}

#[test]
fn ack_rate_bitmaps() {
    // 2.4 GHz b/g AP: basic 1, 2, 5.5, 11.
    assert_eq!(ack_rates(&[0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24], true), (0xf, 0x15));
    // g-only AP: basic 6, 12, 24 (no CCK basic) -> CCK all mandatory added.
    assert_eq!(ack_rates(&[0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c], true), (0xf, 0x15));
    // 5 GHz AP with basic 24 only -> 6, 12 added.
    assert_eq!(ack_rates(&[0x0c, 0x12, 0x18, 0x24, 0xb0, 0x48, 0x60, 0x6c], false).1, 0x15);
    // basic 11 only -> 1, 2, 5.5 added.
    assert_eq!(ack_rates(&[0x96], true).0, 0xf);
}

#[test]
fn binding_quota_time_event() {
    let b = binding_cmd(0, FW_CTXT_ACTION_ADD, &[0]);
    assert_eq!(b.len(), 24);
    assert_eq!(u32_at(&b, 8), 0);
    assert_eq!(u32_at(&b, 12), FW_CTXT_INVALID);
    assert_eq!(u32_at(&b, 20), 0);
    let q = time_quota_cmd(Some(0));
    assert_eq!(q.len(), 64);
    assert_eq!((u32_at(&q, 0), u32_at(&q, 4)), (0, 128));
    assert_eq!(u32_at(&q, 16), FW_CTXT_INVALID);
    assert!(time_quota_cmd(None).chunks(16).all(|c| u32_at(c, 0) == FW_CTXT_INVALID));
    let t = time_event_cmd(0, FW_CTXT_ACTION_ADD, 0, 1000, 500);
    assert_eq!(t.len(), 36);
    assert_eq!(u32_at(&t, 8), TE_BSS_STA_AGGRESSIVE_ASSOC);
    assert_eq!(u32_at(&t, 16), 500);
    assert_eq!(u32_at(&t, 28), 1000);
    assert_eq!(&t[32..36], &[1, 0, 0x03, 0x08]);
    assert_eq!(u32_at(&time_event_cmd(0, FW_CTXT_ACTION_REMOVE, 0x42, 0, 0), 8), 0x42);
    let mut r = vec![0u8; 16];
    r[8] = 9;
    assert_eq!(parse_time_event_resp(&r), Some((0, 9)));
    let mut n = vec![0u8; 24];
    n[8] = 9;
    n[16] = 1;
    n[20] = 1;
    assert_eq!(parse_time_event_notif(&n), Some((9, 1, 1)));
}

#[test]
fn keys_rates_lq() {
    let k = add_sta_key_ccmp(1, 0, 0, &[0xaa; 16], false);
    assert_eq!(k.len(), 64);
    assert_eq!(&k[0..4], &[1, 0, 0x0a, 0x00]); // sta 1, offset 0, CCM | KEY_MAP, key id 0
    assert_eq!(&k[4..20], &[0xaa; 16]);
    let g = add_sta_key_ccmp(1, 1, 2, &[0xbb; 16], true);
    assert_eq!(&g[0..4], &[1, 1, 0x0a, 0x42]); // key id 2, multicast
    assert_eq!(legacy_rate(2), Some(10 | RATE_MCS_CCK | RATE_MCS_ANT_A));
    assert_eq!(legacy_rate(0x8c), Some(13 | RATE_MCS_ANT_A));
    assert_eq!(legacy_rate(108), Some(3 | RATE_MCS_ANT_A));
    assert_eq!(legacy_rate(5), None);
    assert_eq!(mgmt_rate(6), 10 | RATE_MCS_CCK | RATE_MCS_ANT_A);
    assert_eq!(mgmt_rate(36), 13 | RATE_MCS_ANT_A);
    let lq = lq_cmd_legacy(1, &[0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c], 0);
    assert_eq!(lq.len(), 88);
    assert_eq!(lq[0], 1);
    assert_eq!(lq[6], 1); // single-stream antenna A
    let table: Vec<u32> = (0..16).map(|i| u32_at(&lq, 20 + 4 * i)).collect();
    assert_eq!(table[0], legacy_rate(108).unwrap());
    assert_eq!(table[1], legacy_rate(108).unwrap());
    assert_eq!(table[2], legacy_rate(96).unwrap());
    assert_eq!(table[15], legacy_rate(12).unwrap());
    let capped = lq_cmd_legacy(1, &[0x8c, 0x6c], 48);
    assert_eq!(u32_at(&capped, 20), legacy_rate(12).unwrap());
}

#[test]
fn tx_command_and_response() {
    let hdr = [0x08u8, 0x41, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 0, 0];
    let p = TxParams { sta_id: 1, rate: None, tid: TID_NON_QOS, retries: 15, pm_frame_timeout: PM_FRAME_NONE, ccmp_key: Some([7; 16]) };
    let c = tx_cmd(&p, 24 + 8 + 100, &hdr);
    assert_eq!(c.len(), TX_CMD_LEN + 24);
    assert_eq!(u16::from_le_bytes([c[0], c[1]]), 132);
    assert_eq!(u32_at(&c, 4), TX_CMD_FLG_ACK | TX_CMD_FLG_SEQ_CTL | TX_CMD_FLG_STA_RATE);
    assert_eq!((c[16], c[17]), (1, TX_CMD_SEC_CCM));
    assert_eq!(&c[20..36], &[7; 16]);
    assert_eq!(u32_at(&c, 40), 0xffff_ffff);
    assert_eq!((c[49], c[50], c[51]), (60, 15, 0));
    assert_eq!(&c[56..], &hdr);
    let m = tx_cmd(&TxParams { sta_id: 1, rate: Some(mgmt_rate(1)), tid: TID_MGMT, retries: 3, pm_frame_timeout: PM_FRAME_MGMT, ccmp_key: None }, 30, &hdr);
    assert_eq!(u32_at(&m, 4), TX_CMD_FLG_ACK | TX_CMD_FLG_SEQ_CTL);
    assert_eq!(u32_at(&m, 12), mgmt_rate(1));
    assert_eq!(m[17], 0);
    assert_eq!(u16::from_le_bytes([m[52], m[53]]), PM_FRAME_MGMT);

    let mut r = vec![0u8; 44];
    r[0] = 1;
    r[3] = 2;
    r[36] = TX_STATUS_SUCCESS;
    r[40..44].copy_from_slice(&0x0000_0042u32.to_le_bytes());
    let t = parse_tx_resp(&r).unwrap();
    assert!(t.ok());
    assert_eq!((t.failure_frame, t.ssn), (2, 0x42));
    r[36] = 0x83; // FAIL_LONG_LIMIT
    assert!(!parse_tx_resp(&r).unwrap().ok());
    assert!(parse_tx_resp(&r[..40]).is_none());
}

#[test]
fn ccmp_header_and_tim() {
    assert_eq!(ccmp_header(0x0605_0403_0201, 0), [0x01, 0x02, 0, 0x20, 0x03, 0x04, 0x05, 0x06]);
    assert_eq!(ccmp_header(1, 1)[3], 0x60);
    let ies = [0u8, 3, b'a', b'b', b'c', 5, 4, 1, 3, 0, 0];
    assert_eq!(tim_dtim(&ies), Some((1, 3)));
    assert_eq!(tim_dtim(&ies[..7]), None);
}
