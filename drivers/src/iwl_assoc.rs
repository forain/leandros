//! MVM firmware commands for association and the data path (iwlwifi W3):
//! the BSS-station MAC context, binding, time quota, session protection
//! (time event), station keys, the legacy rate table (LQ), the TX command
//! and its response. Hardware independent and host tested
//! (drivers/iwlwifi-tests); layouts follow Linux's iwlwifi `fw/api/*.h` for
//! the versions firmware API 36 speaks (MAC_CONTEXT v1, BINDING v1,
//! TIME_QUOTA v2, TIME_EVENT v2, ADD_STA_KEY v1, LQ v1, TX_CMD v6,
//! TX_RSP v3, rate_n_flags v1).

use alloc::vec::Vec;

use crate::iwl_cmd::{id_and_color, le16, le32, Buf, FW_CTXT_ACTION_REMOVE};

pub const FW_CTXT_INVALID: u32 = 0xffff_ffff;
pub const FW_MAC_TYPE_BSS_STA: u32 = 5;

/// What the BSS-station MAC context says about the association.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacState {
    pub own: [u8; 6],
    pub bssid: [u8; 6],
    pub assoc: bool,
    pub aid: u16,
    pub beacon_int: u32,
    pub dtim_period: u32,
    pub cck_ack: u32,
    pub ofdm_ack: u32,
    pub short_slot: bool,
    pub short_preamble: bool,
    pub use_cts_prot: bool,
    pub dtim_tsf: u64,
    pub dtim_time: u32,
    pub beacon_arrive_time: u32,
}

impl MacState {
    pub fn unassoc(own: [u8; 6]) -> MacState {
        MacState { own, bssid: [0xff; 6], assoc: false, aid: 0, beacon_int: 100, dtim_period: 1,
                   cck_ack: 0xf, ofdm_ack: 0x15, short_slot: false, short_preamble: false, use_cts_prot: false,
                   dtim_tsf: 0, dtim_time: 0, beacon_arrive_time: 0 }
    }
}

/// MAC_CONTEXT_CMD (v1, 148 bytes) for a BSS station.
pub fn mac_ctxt_cmd(mac_id: u32, action: u32, s: &MacState) -> Vec<u8> {
    let mut b = Buf::new();
    b.u32(id_and_color(mac_id, 0)).u32(action);
    b.u32(FW_MAC_TYPE_BSS_STA).u32(0); // tsf_id 0
    b.bytes(&s.own).u16(0).bytes(&s.bssid).u16(0);
    b.u32(s.cck_ack).u32(s.ofdm_ack);
    b.u32(if s.use_cts_prot { 1 << 3 } else { 0 }); // MAC_PROT_FLG_TGG_PROTECT
    b.u32(if s.short_preamble { 1 << 5 } else { 0 });
    b.u32(if s.short_slot { 1 << 4 } else { 0 });
    // MAC_FILTER_ACCEPT_GRP always; MAC_FILTER_IN_BEACON while unassociated.
    b.u32((1 << 2) | if s.assoc { 0 } else { 1 << 6 });
    b.u32(0); // qos_flags (no WMM negotiated)
    // EDCA defaults per firmware AC (BK, BE, VI, VO): cw_min, cw_max, aifsn,
    // FIFO mask, txop in 32 us units.
    for &(cwmin, cwmax, aifs, fifo, txop) in &[(15u16, 1023u16, 7u8, 0u8, 0u16), (15, 1023, 3, 1, 0), (7, 15, 2, 2, 94), (3, 7, 2, 3, 47)] {
        b.u16(cwmin).u16(cwmax).u8(aifs).u8(1 << fifo).u16(txop);
    }
    b.zeros(8); // ac[AC_NUM]
    // struct iwl_mac_data_sta (44 bytes) in a 48-byte union.
    b.u32(s.assoc as u32).u32(s.dtim_time).u64(s.dtim_tsf);
    b.u32(s.beacon_int).u32(0).u32(s.beacon_int * s.dtim_period).u32(0);
    b.u32(10).u32(s.aid as u32).u32(s.beacon_arrive_time);
    b.zeros(4);
    b.done()
}

/// The firmware's ACK-rate bitmaps from a BSS basic rate set (Supported
/// Rates in 500 kb/s units with the 0x80 basic flag), as iwl_mvm_ack_rates:
/// (cck bits 1/2/5.5/11 M, ofdm bits 6..54 M) plus the mandatory lower rates.
pub fn ack_rates(rates: &[u8], band_24: bool) -> (u32, u32) {
    const CCK: [u8; 4] = [2, 4, 11, 22];
    const OFDM: [u8; 8] = [12, 18, 24, 36, 48, 72, 96, 108];
    let mut cck = 0u32;
    let mut ofdm = 0u32;
    for &r in rates.iter().filter(|&&r| r & 0x80 != 0) {
        let r = r & 0x7f;
        if let Some(i) = CCK.iter().position(|&c| c == r) { if band_24 { cck |= 1 << i; } }
        if let Some(i) = OFDM.iter().position(|&c| c == r) { ofdm |= 1 << i; }
    }
    let lowest_ofdm = (0..8).find(|i| ofdm & (1 << i) != 0).unwrap_or(8);
    if lowest_ofdm > 4 { ofdm |= 1 << 4; } // 24M
    if lowest_ofdm > 2 { ofdm |= 1 << 2; } // 12M
    ofdm |= 1; // 6M
    let lowest_cck = (0..4).find(|i| cck & (1 << i) != 0).unwrap_or(4);
    if lowest_cck > 3 { cck |= 1 << 3; }
    if lowest_cck > 2 { cck |= 1 << 2; }
    if lowest_cck > 1 { cck |= 1 << 1; }
    cck |= 1;
    (cck, ofdm)
}

/// BINDING_CONTEXT_CMD v1: bind `macs` to PHY context `phy`.
pub fn binding_cmd(phy: u32, action: u32, macs: &[u32]) -> Vec<u8> {
    let mut b = Buf::new();
    b.u32(id_and_color(phy, 0)).u32(action);
    for i in 0..3 { b.u32(macs.get(i).map_or(FW_CTXT_INVALID, |&m| id_and_color(m, 0))); }
    b.u32(id_and_color(phy, 0));
    b.done()
}

/// TIME_QUOTA_CMD (v2, with the low-latency field): the whole session (128
/// fragments) to `binding`, or no data binding at all.
pub fn time_quota_cmd(binding: Option<u32>) -> Vec<u8> {
    let mut b = Buf::new();
    for i in 0..4 {
        match (i, binding) {
            (0, Some(id)) => { b.u32(id_and_color(id, 0)).u32(128).u32(0).u32(0); }
            _ => { b.u32(FW_CTXT_INVALID).u32(0).u32(0).u32(0); }
        }
    }
    b.done()
}

pub const TE_BSS_STA_AGGRESSIVE_ASSOC: u32 = 0;
pub const TE_V2_NOTIF_HOST_EVENT_START: u16 = 1 << 0;
pub const TE_V2_NOTIF_HOST_EVENT_END: u16 = 1 << 1;
pub const TE_V2_START_IMMEDIATELY: u16 = 1 << 11;

/// TIME_EVENT_CMD (v2): keep the radio on our channel for `duration_tu`
/// (session protection around authentication and association).
pub fn time_event_cmd(mac_id: u32, action: u32, unique_id: u32, duration_tu: u32, max_delay_tu: u32) -> Vec<u8> {
    let mut b = Buf::new();
    b.u32(id_and_color(mac_id, 0)).u32(action);
    b.u32(if action == FW_CTXT_ACTION_REMOVE { unique_id } else { TE_BSS_STA_AGGRESSIVE_ASSOC });
    b.u32(0).u32(max_delay_tu).u32(0).u32(1).u32(duration_tu);
    b.u8(1).u8(0).u16(TE_V2_NOTIF_HOST_EVENT_START | TE_V2_NOTIF_HOST_EVENT_END | TE_V2_START_IMMEDIATELY);
    b.done()
}

/// TIME_EVENT_CMD response: (status, unique_id).
pub fn parse_time_event_resp(p: &[u8]) -> Option<(u32, u32)> { Some((le32(p, 0)?, le32(p, 8)?)) }
/// TIME_EVENT_NOTIFICATION: (unique_id, action, status); action bit 0 = start, bit 1 = end.
pub fn parse_time_event_notif(p: &[u8]) -> Option<(u32, u32, u32)> { Some((le32(p, 8)?, le32(p, 16)?, le32(p, 20)?)) }

// ── keys and rates ──────────────────────────────────────────────────────────

pub const STA_KEY_FLG_CCM: u16 = 2;
pub const STA_KEY_FLG_WEP_KEY_MAP: u16 = 1 << 3;
pub const STA_KEY_MULTICAST: u16 = 1 << 14;

/// ADD_STA_KEY (v1, 64 bytes): a CCMP key for `sta_id` at firmware key slot
/// `offset`; `mcast` for the group key. The receive PN starts at zero.
pub fn add_sta_key_ccmp(sta_id: u8, offset: u8, key_idx: u8, key: &[u8; 16], mcast: bool) -> Vec<u8> {
    let flags = ((key_idx as u16 & 3) << 8) | STA_KEY_FLG_WEP_KEY_MAP | STA_KEY_FLG_CCM | if mcast { STA_KEY_MULTICAST } else { 0 };
    let mut b = Buf::new();
    b.u8(sta_id).u8(offset).u16(flags);
    let mut k = [0u8; 32];
    k[..16].copy_from_slice(key);
    b.bytes(&k).zeros(16); // key, rx_secur_seq_cnt
    b.u8(0).u8(0).zeros(10); // TKIP fields
    b.done()
}

pub const RATE_MCS_CCK: u32 = 1 << 9;
pub const RATE_MCS_ANT_POS: u32 = 14;
pub const RATE_MCS_ANT_A: u32 = 1 << 14;
pub const RATE_MCS_ANT_B: u32 = 2 << 14;
pub const RATE_MCS_ANT_MSK: u32 = 7 << 14;
/// Antenna masks (valid_tx_ant, LQ single_stream_ant_msk).
pub const ANT_A: u8 = 1;
pub const ANT_B: u8 = 2;

/// The legacy rates in 500 kb/s units, slowest first (by nominal rate:
/// 1, 2, 5.5, 6, 9, 11, 12, 18, 24, 36, 48, 54 Mb/s).
pub const LEGACY_R500K: [u8; 12] = [2, 4, 11, 12, 18, 22, 24, 36, 48, 72, 96, 108];

fn legacy_plcp(r500k: u8) -> Option<(u32, bool)> {
    Some(match r500k & 0x7f {
        2 => (10, true), 4 => (20, true), 11 => (55, true), 22 => (110, true),
        12 => (13, false), 18 => (15, false), 24 => (5, false), 36 => (7, false),
        48 => (9, false), 72 => (11, false), 96 => (1, false), 108 => (3, false),
        _ => return None,
    })
}

/// rate_n_flags (v1 format) of a legacy rate in 500 kb/s units on antenna
/// mask `ant` (ANT_A / ANT_B).
pub fn legacy_rate_ant(r500k: u8, ant: u8) -> Option<u32> {
    let (plcp, cck) = legacy_plcp(r500k)?;
    Some(plcp | if cck { RATE_MCS_CCK } else { 0 } | ((ant as u32 & 7) << RATE_MCS_ANT_POS))
}

/// rate_n_flags (v1 format) of a legacy rate in 500 kb/s units, antenna A
/// (the antenna Bluetooth does not share on this family).
pub fn legacy_rate(r500k: u8) -> Option<u32> { legacy_rate_ant(r500k, ANT_A) }

/// The legacy rate (500 kb/s units) a v1 rate_n_flags carries, any antenna.
pub fn rate_r500k(rate_n_flags: u32) -> Option<u8> {
    if rate_n_flags & (1 << 8) != 0 { return None; } // HT
    let cck = rate_n_flags & RATE_MCS_CCK != 0;
    let plcp = rate_n_flags & 0xff;
    LEGACY_R500K.iter().copied().find(|&r| legacy_plcp(r) == Some((plcp, cck)))
}

/// The bits of a rate_n_flags that identify a legacy rate on an antenna
/// (what the TX response's initial rate is compared on).
pub const RATE_ID_MSK: u32 = 0xff | (1 << 8) | RATE_MCS_CCK | RATE_MCS_ANT_MSK;

/// The lowest mandatory rate of the band (management frames).
pub fn mgmt_rate(channel: u8) -> u32 { legacy_rate(if channel <= 14 { 2 } else { 12 }).unwrap() }

/// LQ_CMD (v1, 88 bytes) for a legacy (non-HT) station with an explicit
/// 16-entry retry table; `ss_ant` is the single-stream antenna mask.
pub fn lq_cmd_table(sta_id: u8, table: &[u32; 16], ss_ant: u8) -> Vec<u8> {
    let mut b = Buf::new();
    b.u8(sta_id).u8(0).u16(0);
    b.u8(0).u8(0).u8(ss_ant).u8(3); // flags, mimo_delim, single-stream ant, dual-stream AB
    b.u8(0).u8(0).u8(0).u8(0); // initial_rate_index[4]
    b.u16(4000).u8(3).u8(64); // agg_time_limit, agg_disable_start_th, agg_frame_cnt_limit
    b.u32(0);
    for &r in table { b.u32(r); }
    b.u32(0); // ss_params
    b.done()
}

/// LQ_CMD (v1, 88 bytes) for a legacy (non-HT) station: the AP's rates,
/// fastest first, each tried twice, the slowest repeated to fill 16 slots.
/// `max_r500k` caps the fastest rate (0 = no cap). The fixed table used
/// before rate control (iwlwifi.rc=0).
pub fn lq_cmd_legacy(sta_id: u8, rates: &[u8], max_r500k: u8) -> Vec<u8> {
    let mut rs: Vec<u8> = rates.iter().map(|r| r & 0x7f)
        .filter(|&r| legacy_rate(r).is_some() && (max_r500k == 0 || r <= max_r500k)).collect();
    rs.sort_unstable_by(|a, b| b.cmp(a));
    rs.dedup();
    if rs.is_empty() { rs.push(12); }
    let mut table = Vec::with_capacity(16);
    for &r in &rs { for _ in 0..2 { if table.len() < 16 { table.push(legacy_rate(r).unwrap()); } } }
    let last = *table.last().unwrap();
    while table.len() < 16 { table.push(last); }
    let mut t = [0u32; 16];
    t.copy_from_slice(&table);
    lq_cmd_table(sta_id, &t, ANT_A)
}

// ── TX ──────────────────────────────────────────────────────────────────────

pub const TX_CMD_LEN: usize = 56;
pub const TX_CMD_FLG_ACK: u32 = 1 << 3;
pub const TX_CMD_FLG_STA_RATE: u32 = 1 << 4;
pub const TX_CMD_FLG_SEQ_CTL: u32 = 1 << 13;
pub const TX_CMD_SEC_CCM: u8 = 2;
pub const PM_FRAME_NONE: u16 = 0;
pub const PM_FRAME_MGMT: u16 = 2;
pub const PM_FRAME_ASSOC: u16 = 3;
pub const TID_NON_QOS: u8 = 0;
pub const TID_MGMT: u8 = 8;

pub struct TxParams {
    pub sta_id: u8,
    /// None: use the station's LQ table (TX_CMD_FLG_STA_RATE).
    pub rate: Option<u32>,
    pub tid: u8,
    pub retries: u8,
    pub pm_frame_timeout: u16,
    /// CCMP temporal key: the firmware encrypts (the caller put the CCMP
    /// header in the frame; the firmware appends the MIC).
    pub ccmp_key: Option<[u8; 16]>,
}

/// TX_CMD (v6) followed by the 802.11 header `hdr`; `mpdu_len` = header +
/// body (including a CCMP header, excluding the MIC the firmware appends).
pub fn tx_cmd(p: &TxParams, mpdu_len: usize, hdr: &[u8]) -> Vec<u8> {
    let mut flags = TX_CMD_FLG_ACK | TX_CMD_FLG_SEQ_CTL;
    if p.rate.is_none() { flags |= TX_CMD_FLG_STA_RATE; }
    let mut b = Buf::new();
    b.u16(mpdu_len as u16).u16(0).u32(flags);
    b.u8(0).u8(0).u16(0); // scratch
    b.u32(p.rate.unwrap_or(0));
    b.u8(p.sta_id).u8(if p.ccmp_key.is_some() { TX_CMD_SEC_CCM } else { 0 }).u8(0).u8(0);
    b.bytes(&p.ccmp_key.unwrap_or([0; 16]));
    b.u32(0).u32(0xffff_ffff); // reserved3, life_time infinite
    b.u32(0).u8(0); // DRAM scratch pointers
    b.u8(60).u8(p.retries).u8(p.tid);
    b.u16(p.pm_frame_timeout).u16(0);
    b.bytes(hdr);
    b.done()
}

pub const TX_STATUS_SUCCESS: u8 = 0x01;
pub const TX_STATUS_DIRECT_DONE: u8 = 0x02;
pub const TX_STATUS_FAIL_SHORT_LIMIT: u8 = 0x82;
pub const TX_STATUS_FAIL_LONG_LIMIT: u8 = 0x83;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxResp {
    pub frame_count: u8,
    /// Attempts Bluetooth coexistence killed.
    pub bt_kill_count: u8,
    /// RTS attempts that got no CTS.
    pub failure_rts: u8,
    /// Data attempts that failed before the final one (retries).
    pub failure_frame: u8,
    pub initial_rate: u32,
    /// The firmware lowered the TX power (thermal / SAR).
    pub reduced_tpc: u8,
    pub status: u16,
    pub seq_ctl: u16,
    pub ssn: u16,
}

impl TxResp {
    pub fn ok(&self) -> bool { matches!((self.status & 0xff) as u8, TX_STATUS_SUCCESS | TX_STATUS_DIRECT_DONE) }
    /// The frame went out and was never acknowledged (retry limits): the
    /// failures rate control learns from. Postponed, flushed, dropped frames
    /// say nothing about the rate.
    pub fn air_fail(&self) -> bool { matches!((self.status & 0xff) as u8, TX_STATUS_FAIL_SHORT_LIMIT | TX_STATUS_FAIL_LONG_LIMIT) }
}

/// TX_CMD response (TX_RSP_API_S_VER_3, single frame): status of the frame
/// and the scheduler SSN that follows the status array. Layout: frame_count,
/// bt_kill_count, failure_rts, failure_frame, initial_rate (4), wireless
/// media time (2), pa_status, pa_integ a/b/c (9), measurement_req_id (2),
/// reduced_tpc (22), reserved, tfd_info (24), seq_ctl (28), byte_cnt,
/// tlc_info, ra_tid, frame_ctrl, status[] (36), then the SSN.
pub fn parse_tx_resp(p: &[u8]) -> Option<TxResp> {
    let n = *p.first()? as usize;
    let status = le16(p, 36)?;
    let ssn = (le32(p, 36 + 4 * n.max(1))? & 0xfff) as u16;
    Some(TxResp { frame_count: n as u8, bt_kill_count: *p.get(1)?, failure_rts: *p.get(2)?, failure_frame: *p.get(3)?,
                  initial_rate: le32(p, 4)?, reduced_tpc: *p.get(22)?, status, seq_ctl: le16(p, 28)?, ssn })
}

/// The ERP element's flags (802.11 9.4.2.11: bit 0 NonERP_Present, bit 1
/// Use_Protection, bit 2 Barker_Preamble_Mode) from a beacon's IEs.
pub fn erp_ie(ies: &[u8]) -> Option<u8> {
    let mut i = 0;
    while i + 2 <= ies.len() {
        let id = ies[i];
        let len = ies[i + 1] as usize;
        if i + 2 + len > ies.len() { return None; }
        if id == 42 && len >= 1 { return Some(ies[i + 2]); }
        i += 2 + len;
    }
    None
}
pub const ERP_USE_PROTECTION: u8 = 1 << 1;
pub const ERP_BARKER_PREAMBLE: u8 = 1 << 2;

/// The CCMP header (802.11 12.5.3.2) for packet number `pn` and key id.
pub fn ccmp_header(pn: u64, key_id: u8) -> [u8; 8] {
    [pn as u8, (pn >> 8) as u8, 0, 0x20 | (key_id << 6), (pn >> 16) as u8, (pn >> 24) as u8, (pn >> 32) as u8, (pn >> 40) as u8]
}

/// The TIM element's (DTIM count, DTIM period) from a beacon's IEs.
pub fn tim_dtim(ies: &[u8]) -> Option<(u8, u8)> {
    let mut i = 0;
    while i + 2 <= ies.len() {
        let id = ies[i];
        let len = ies[i + 1] as usize;
        if i + 2 + len > ies.len() { return None; }
        if id == 5 && len >= 2 { return Some((ies[i + 2], ies[i + 3])); }
        i += 2 + len;
    }
    None
}
