use crate::iwl_cmd::*;

#[test]
fn command_sizes_match_the_firmware_structs() {
    // Sizes of the fw/api structs for the versions API 36 speaks.
    let sta = AddSta {
        modify: false, sta_id: 1, mac_id_n_color: 0, addr: [1, 2, 3, 4, 5, 6], station_type: STA_LINK,
        tid_disable_tx: 0xffff, modify_mask: 0, station_flags: 0, station_flags_msk: 0, assoc_id: 0, tfd_queue_msk: 0x10,
    };
    let a = add_sta_cmd(&sta);
    assert_eq!(a.len(), 48);
    assert_eq!(&a[8..14], &[1, 2, 3, 4, 5, 6]);
    assert_eq!(a[16], 1); // sta_id
    assert_eq!(&a[40..44], &0x10u32.to_le_bytes()); // tfd_queue_msk
    assert_eq!(phy_ctxt_cmd(0, 0, FW_CTXT_ACTION_ADD, 6, 3, 3).len(), 36);
    assert_eq!(scd_queue_cfg_cmd(4, true, 1, 8, 3, 0).len(), 12);
    assert_eq!(phy_cfg_cmd(0, 0, 0).len(), 12);
    assert_eq!(sf_cfg_cmd(SF_INIT_OFF, None).len(), 92);
    assert_eq!(mcc_update_cmd(*b"ZZ", MCC_SOURCE_GET_CURRENT).len(), 28);
    assert_eq!(paging_cmd(5, 7, &[0x1000, 0x8000]).len(), 12 + 33 * 4);
    assert_eq!(nvm_read_cmd(1, 0, 2048).len(), 8);
    assert_eq!(bt_config_cmd(true), vec![1, 0, 0, 0, 0x15, 0, 0, 0]);
    assert_eq!(temp_report_ths_cmd().len(), 20);
    assert_eq!(device_power_cmd(false), vec![0, 0, 0, 0]);
    // Scan: config v1 = 36 bytes + the channel array; request v7 = 48 +
    // channel configs + tail v1 (8 + 4 + 528 + 20 * 34).
    let ch = [1u8, 6, 11, 36];
    assert_eq!(scan_config_v1(3, 3, &[0; 6], 0, &ch, 52).len(), 36 + 52);
    let r = scan_req_umac_v7(0, 0, &[2; 6], &ch, 52, &[b"home"], false);
    assert_eq!(r.len(), 48 + 52 * 8 + 8 + 4 + 528 + 680);
}

#[test]
fn phy_ctxt_layout() {
    let c = phy_ctxt_cmd(0, 0, FW_CTXT_ACTION_MODIFY, 36, 3, 3);
    assert_eq!(&c[4..8], &2u32.to_le_bytes());
    assert_eq!(&c[16..20], &[PHY_BAND_5, 36, 0, 0]);
    assert_eq!(&c[20..24], &3u32.to_le_bytes()); // tx chains
    let rx = u32::from_le_bytes([c[24], c[25], c[26], c[27]]);
    assert_eq!(rx, (3 << 1) | (2 << 10) | (2 << 12));
    let c = phy_ctxt_cmd(0, 0, FW_CTXT_ACTION_ADD, 1, 1, 1);
    assert_eq!(c[16], PHY_BAND_24);
    let rx = u32::from_le_bytes([c[24], c[25], c[26], c[27]]);
    assert_eq!(rx, (1 << 1) | (1 << 10) | (1 << 12));
}

#[test]
fn scan_request_layout() {
    let ch = [1u8, 6, 11];
    let r = scan_req_umac_v7(3, 0, &[2, 0, 0, 0, 0, 1], &ch, 52, &[b"myssid"], false);
    assert_eq!(&r[4..8], &3u32.to_le_bytes()); // uid
    let gen = u16::from_le_bytes([r[12], r[13]]);
    assert_eq!(gen, SCAN_GEN_FLAGS_PASS_ALL | SCAN_GEN_FLAGS_ADAPTIVE_DWELL);
    assert_eq!(r[16], 10); // active dwell
    assert_eq!(r[17], 110); // passive dwell
    assert_eq!(&r[44..46], &[0, 3]); // channel params: flags, count
    // First channel config: SSID bitmap (wildcard + myssid), channel 1, 1 iteration.
    assert_eq!(&r[48..56], &[3, 0, 0, 0, 1, 1, 0, 0]);
    assert_eq!(&r[56..64], &[3, 0, 0, 0, 6, 1, 0, 0]);
    let tail = 48 + 52 * 8;
    assert_eq!(&r[tail..tail + 4], &[0, 0, 1, 0]); // schedule[0]: interval 0, 1 iteration
    let preq = tail + 12;
    // mac header segment: offset 0, length 26 (24-byte header + empty SSID IE)
    assert_eq!(&r[preq..preq + 4], &[0, 0, 26, 0]);
    let buf = preq + 16;
    assert_eq!(&r[buf..buf + 2], &[0x40, 0]); // probe request
    assert_eq!(&r[buf + 10..buf + 16], &[2, 0, 0, 0, 0, 1]); // SA
    let ssids = buf + 512;
    assert_eq!(&r[ssids..ssids + 2], &[0, 0]); // wildcard
    assert_eq!(&r[ssids + 34..ssids + 42], &[0, 6, b'm', b'y', b's', b's', b'i', b'd']);
    // Passive: no SSID bitmap, PASSIVE flag.
    let p = scan_req_umac_v7(0, 0, &[2; 6], &ch, 52, &[], true);
    assert_ne!(u16::from_le_bytes([p[12], p[13]]) & SCAN_GEN_FLAGS_PASSIVE, 0);
    assert_eq!(&p[48..52], &[0, 0, 0, 0]);
}

#[test]
fn alive_v3_and_v4() {
    let mut v3 = vec![0u8; 68];
    v3[0..2].copy_from_slice(&0xcafeu16.to_le_bytes());
    v3[4..8].copy_from_slice(&36u32.to_le_bytes());
    v3[13] = 1; // ver_type
    v3[20..24].copy_from_slice(&0x0080_0a00u32.to_le_bytes()); // error table
    v3[40..44].copy_from_slice(&0x0080_0000u32.to_le_bytes()); // scd base
    v3[52..56].copy_from_slice(&5u32.to_le_bytes()); // umac major
    v3[60..64].copy_from_slice(&0xc080_1000u32.to_le_bytes()); // umac error table (cache bits)
    let a = parse_alive(&v3).unwrap();
    assert_eq!((a.status, a.ucode_major, a.ver_type, a.lmacs), (ALIVE_STATUS_OK, 36, 1, 1));
    assert_eq!((a.lmac_error_table, a.scd_base, a.umac_major, a.umac_error_table), (0x800a00, 0x800000, 5, 0x801000));
    let mut v4 = vec![0u8; 116];
    v4[0..2].copy_from_slice(&0xcafeu16.to_le_bytes());
    v4[100..104].copy_from_slice(&7u32.to_le_bytes());
    let a = parse_alive(&v4).unwrap();
    assert_eq!((a.lmacs, a.umac_major), (2, 7));
    assert!(parse_alive(&[0u8; 60]).is_none());
}

#[test]
fn nvm_8000_parse() {
    let mut sw = vec![0u8; 16];
    sw[0..2].copy_from_slice(&0x0e3fu16.to_le_bytes());
    let mut sku = vec![0u8; 8];
    sku[0..4].copy_from_slice(&0x3300_0000u32.to_le_bytes()); // tx ant 3, rx ant 3
    sku[4..8].copy_from_slice(&0x0000_000fu32.to_le_bytes()); // 2.4 + 5 + 11n + 11ac
    let mut reg = vec![0u8; 2 * 0x508];
    let valid_active = (chan_flags::VALID | chan_flags::ACTIVE).to_le_bytes();
    for i in 0..13 { reg[2 * i..2 * i + 2].copy_from_slice(&valid_active); }
    reg[2 * 14..2 * 14 + 2].copy_from_slice(&chan_flags::VALID.to_le_bytes()); // 36 passive
    reg[2 * 0x507..2 * 0x507 + 2].copy_from_slice(&7u16.to_le_bytes()); // LAR
    let mut mo = vec![0u8; 8];
    mo[2..8].copy_from_slice(&[0x02, 0xcc, 0xaa, 0xff, 0xee, 0x00]); // reserved placeholder
    let d = parse_nvm_8000(&sw, &reg, &sku, Some(&mo)).unwrap();
    assert_eq!(d.nvm_version, 0x0e3f);
    assert_eq!((d.valid_tx_ant, d.valid_rx_ant), (3, 3));
    assert!(d.band_24 && d.band_52 && d.ht && d.vht && d.lar_enabled);
    assert_eq!(d.mac_override, None);
    assert_eq!(d.channels.len(), 14);
    assert_eq!(d.channels[13], Channel { ch: 36, flags: chan_flags::VALID });
    mo[2..8].copy_from_slice(&[0x34, 0x13, 0xe8, 0x01, 0x02, 0x03]);
    let d = parse_nvm_8000(&sw, &reg, &sku, Some(&mo)).unwrap();
    assert_eq!(d.mac_override, Some([0x34, 0x13, 0xe8, 0x01, 0x02, 0x03]));
    assert_eq!(mac_from_otp(0x3413_e801, 0x0000_0203), [0x34, 0x13, 0xe8, 0x01, 0x02, 0x03]);
    assert!(parse_nvm_8000(&sw, &reg, &sku[..4], None).is_none());
}

#[test]
fn nvm_and_mcc_responses() {
    let mut r = vec![0u8; 8];
    r[2..4].copy_from_slice(&4u16.to_le_bytes());
    r.extend_from_slice(&[1, 2, 3, 4]);
    assert_eq!(parse_nvm_resp(&r), Some((0, 4, 0, 0, &[1u8, 2, 3, 4][..])));
    assert!(parse_nvm_resp(&r[..10]).is_none());

    let mut m = vec![0u8; 16];
    m[4..6].copy_from_slice(&((b'D' as u16) << 8 | b'E' as u16).to_le_bytes());
    m[7] = 0x10;
    m[12..16].copy_from_slice(&2u32.to_le_bytes());
    m.extend_from_slice(&0x9u32.to_le_bytes());
    m.extend_from_slice(&0x1u32.to_le_bytes());
    let p = parse_mcc_resp(&m, false).unwrap();
    assert_eq!(p.mcc, *b"DE");
    assert_eq!(p.source, 0x10);
    assert_eq!(p.channels, vec![9, 1]);
    assert!(parse_mcc_resp(&m[..20], false).is_none());
}

#[test]
fn rx_parsing() {
    let mut phy = vec![0u8; RX_PHY_INFO_LEN];
    phy[20..22].copy_from_slice(&RX_RES_PHY_FLAGS_BAND_24.to_le_bytes());
    phy[22..24].copy_from_slice(&6u16.to_le_bytes());
    phy[28..32].copy_from_slice(&0x0000_3c41u32.to_le_bytes()); // A 65, B 60
    let p = parse_rx_phy(&phy).unwrap();
    assert_eq!(p.channel, 6);
    assert_eq!(p.signal_dbm(), -60);
    assert!(parse_rx_phy(&phy[..60]).is_none());

    let mut mpdu = vec![0u8; 4];
    mpdu[0..2].copy_from_slice(&3u16.to_le_bytes());
    mpdu.extend_from_slice(&[0x80, 0, 0]);
    mpdu.extend_from_slice(&0x0000_0003u32.to_le_bytes());
    let (f, st) = parse_rx_mpdu(&mpdu).unwrap();
    assert_eq!((f, st), (&[0x80u8, 0, 0][..], 3));
    assert!(parse_rx_mpdu(&mpdu[..8]).is_none());

    assert_eq!(rx_crypto_lengths(0x0008, 0), Some((0, 0)));
    assert_eq!(rx_crypto_lengths(0x4008, RX_MPDU_STATUS_SEC_CCM | RX_MPDU_STATUS_MIC_OK), Some((8, 8)));
    assert_eq!(rx_crypto_lengths(0x4008, RX_MPDU_STATUS_SEC_CCM), None);
    assert_eq!(rx_crypto_lengths(0x4008, RX_MPDU_STATUS_SEC_NONE), None);
}

#[test]
fn phy_db_replay_order() {
    let mut db = PhyDb::default();
    db.add(PHY_DB_CALIB_CHG_TXP, &[1, 0, 9]);
    db.add(PHY_DB_CALIB_CHG_TXP, &[0, 0, 8]);
    db.add(PHY_DB_CALIB_CHG_PAPD, &[0, 0, 7]);
    db.add(PHY_DB_CALIB_NCH, &[6]);
    db.add(PHY_DB_CFG, &[5]);
    db.add(PHY_DB_CFG, &[4]); // replaces
    let o = db.ordered();
    let types: Vec<u16> = o.iter().map(|e| e.0).collect();
    assert_eq!(types, vec![PHY_DB_CFG, PHY_DB_CALIB_NCH, PHY_DB_CALIB_CHG_PAPD, PHY_DB_CALIB_CHG_TXP, PHY_DB_CALIB_CHG_TXP]);
    assert_eq!(o[0].1, &[4]);
    assert_eq!(o[3].1, &[0, 0, 8]);
    assert_eq!(phy_db_cmd(1, &[4]), vec![1, 0, 1, 0, 4]);
    assert_eq!(parse_phy_db(&[2, 0, 1, 0, 6, 99]), Some((2, &[6u8][..])));
}

#[test]
fn phy_config_masks_chains() {
    assert_eq!(phy_config(0x0033_0018, 3, 3), 0x0033_0018);
    assert_eq!(phy_config(0x0033_0018, 1, 2), 0x0021_0018);
}

#[test]
fn command_ids_match_linux_commands_h() {
    // fw/api/commands.h (Linux 6.6), legacy group.
    assert_eq!(UCODE_ALIVE_NTFY, 0x01);
    assert_eq!(INIT_COMPLETE_NOTIF, 0x04);
    assert_eq!(SCAN_COMPLETE_UMAC, 0x0f);
    assert_eq!(SHARED_MEM_CFG, 0x25);
    assert_eq!(FW_PAGING_BLOCK_CMD, 0x4f);
    assert_eq!(PHY_CONFIGURATION_CMD, 0x6a);
    assert_eq!(CALIB_RES_NOTIF_PHY_DB, 0x6b);
    assert_eq!(NVM_ACCESS_CMD, 0x88);
    assert_eq!(TX_ANT_CONFIGURATION_CMD, 0x98);
    assert_eq!(BT_CONFIG, 0x9b);
    assert_eq!(LTR_CONFIG, 0xee);
    assert_eq!(LONG_GROUP, 0x01);
}
