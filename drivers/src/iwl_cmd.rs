//! Intel MVM firmware host-command and notification formats used by the
//! iwlwifi driver (drivers/src/iwlwifi.rs): byte-exact encoders for the
//! commands and bounds-checked decoders for the notifications, plus the
//! family-8000 NVM parser. Hardware independent and host tested
//! (drivers/iwlwifi-tests); layouts follow Linux's iwlwifi `fw/api/*.h` for
//! the versions firmware API 36 (iwlwifi-8265-36.ucode) speaks, which the
//! driver checks against the firmware's CMD_VERSIONS / API TLVs at start.
//!
//! All multi-byte fields are little endian.

use alloc::vec::Vec;

// ── command ids (group 0 unless noted) ─────────────────────────────────────

pub const UCODE_ALIVE_NTFY: u8 = 0x01;
pub const REPLY_ERROR: u8 = 0x02;
pub const INIT_COMPLETE_NOTIF: u8 = 0x04;
pub const PHY_CONTEXT_CMD: u8 = 0x08;
pub const SCAN_CFG_CMD: u8 = 0x0c;
pub const SCAN_REQ_UMAC: u8 = 0x0d;
pub const SCAN_ABORT_UMAC: u8 = 0x0e;
pub const SCAN_COMPLETE_UMAC: u8 = 0x0f;
pub const ADD_STA_KEY: u8 = 0x17;
pub const ADD_STA: u8 = 0x18;
pub const REMOVE_STA: u8 = 0x19;
pub const TX_CMD: u8 = 0x1c;
pub const SCD_QUEUE_CFG: u8 = 0x1d;
pub const TXPATH_FLUSH: u8 = 0x1e;
pub const MGMT_MCAST_KEY: u8 = 0x1f;
pub const MAC_CONTEXT_CMD: u8 = 0x28;
pub const TIME_EVENT_CMD: u8 = 0x29;
pub const TIME_EVENT_NOTIFICATION: u8 = 0x2a;
pub const BINDING_CONTEXT_CMD: u8 = 0x2b;
pub const TIME_QUOTA_CMD: u8 = 0x2c;
pub const NON_QOS_TX_COUNTER_CMD: u8 = 0x2d;
pub const LQ_CMD: u8 = 0x4e;
pub const FW_PAGING_BLOCK_CMD: u8 = 0x4f;
pub const PHY_CONFIGURATION_CMD: u8 = 0x6a;
pub const CALIB_RES_NOTIF_PHY_DB: u8 = 0x6b;
pub const PHY_DB_CMD: u8 = 0x6c;
pub const POWER_TABLE_CMD: u8 = 0x77;
pub const PSM_UAPSD_AP_MISBEHAVING_NOTIFICATION: u8 = 0x78;
pub const LTR_CONFIG: u8 = 0xee;
pub const NVM_ACCESS_CMD: u8 = 0x88;
pub const SHARED_MEM_CFG: u8 = 0x25;
pub const TX_ANT_CONFIGURATION_CMD: u8 = 0x98;
pub const STATISTICS_NOTIFICATION: u8 = 0x9d;
pub const BT_CONFIG: u8 = 0x9b;
pub const REDUCE_TX_POWER_CMD: u8 = 0x9f;
pub const MISSED_BEACONS_NOTIFICATION: u8 = 0xa2;
pub const MAC_PM_POWER_TABLE: u8 = 0xa9;
pub const SCAN_ITERATION_COMPLETE_UMAC: u8 = 0xb5;
pub const REPLY_RX_PHY_CMD: u8 = 0xc0;
pub const REPLY_RX_MPDU_CMD: u8 = 0xc1;
pub const BA_NOTIF: u8 = 0xc5;
pub const MCC_UPDATE_CMD: u8 = 0xc8;
pub const MCC_CHUB_UPDATE_CMD: u8 = 0xc9;
pub const MARKER_CMD: u8 = 0xcb;
pub const BEACON_NOTIFICATION: u8 = 0x90;
pub const REPLY_SF_CFG_CMD: u8 = 0xd1;
pub const REPLY_BEACON_FILTERING_CMD: u8 = 0xd2;
pub const DTS_MEASUREMENT_NOTIFICATION: u8 = 0xdd;
pub const DEBUG_LOG_MSG: u8 = 0xf7;
pub const MFUART_LOAD_NOTIFICATION: u8 = 0xb1;

pub const LONG_GROUP: u8 = 0x1;
pub const PHY_OPS_GROUP: u8 = 0x4;
pub const DATA_PATH_GROUP: u8 = 0x5;
// ── host command wire encoding ─────────────────────────────────────────────

/// Group a host command goes out in. MVM sets `trans->wide_cmd_header`, so
/// `iwl_trans_send_cmd` turns every group-0 id except REPLY_ERROR into
/// DEF_ID(id) = LONG_GROUP; ids with their own group keep it.
pub fn hcmd_group(id: u8, group: u8) -> u8 {
    if group == 0 && id != REPLY_ERROR { LONG_GROUP } else { group }
}

/// Command header as `iwl_pcie_enqueue_hcmd` builds it (gen1 transport):
/// group != 0 → 8-byte `iwl_cmd_header_wide` {cmd, group, seq, length =
/// payload bytes, reserved 0, version}; group 0 → 4-byte {cmd, 0, seq}.
/// Returns the header bytes and the header length. The TFD covers exactly
/// header + payload: Linux never pads a command up to IWL_FIRST_TB_SIZE, and
/// trailing pad bytes are parsed by the firmware as a further command.
pub fn hcmd_header(id: u8, group: u8, version: u8, seq: u16, payload_len: usize) -> ([u8; 8], usize) {
    let mut h = [0u8; 8];
    h[0] = id;
    h[1] = group;
    h[2..4].copy_from_slice(&seq.to_le_bytes());
    if group == 0 { return (h, 4); }
    h[4..6].copy_from_slice(&(payload_len as u16).to_le_bytes());
    h[7] = version;
    (h, 8)
}

pub const DQA_ENABLE_CMD: u8 = 0x00;
pub const CTDP_CONFIG_CMD: u8 = 0x03;
pub const TEMP_REPORTING_THRESHOLDS_CMD: u8 = 0x04;
pub const CT_KILL_NOTIFICATION: u8 = 0xfe;
pub const DTS_MEASUREMENT_NOTIF_WIDE: u8 = 0xff;

/// Human-readable name of a group-0 command/notification (for the log).
pub fn cmd_name(group: u8, cmd: u8) -> &'static str {
    if group == PHY_OPS_GROUP {
        return match cmd { CT_KILL_NOTIFICATION => "CT_KILL_NOTIF", DTS_MEASUREMENT_NOTIF_WIDE => "DTS_MEASUREMENT_NOTIF", _ => "PHY_OPS_GROUP cmd" };
    }
    match cmd {
        UCODE_ALIVE_NTFY => "ALIVE", REPLY_ERROR => "REPLY_ERROR", INIT_COMPLETE_NOTIF => "INIT_COMPLETE",
        PHY_CONTEXT_CMD => "PHY_CONTEXT", SCAN_CFG_CMD => "SCAN_CFG", SCAN_REQ_UMAC => "SCAN_REQ_UMAC",
        SCAN_ABORT_UMAC => "SCAN_ABORT_UMAC", SCAN_COMPLETE_UMAC => "SCAN_COMPLETE_UMAC", ADD_STA_KEY => "ADD_STA_KEY",
        ADD_STA => "ADD_STA", REMOVE_STA => "REMOVE_STA", TX_CMD => "TX_CMD", SCD_QUEUE_CFG => "SCD_QUEUE_CFG",
        TXPATH_FLUSH => "TXPATH_FLUSH", MAC_CONTEXT_CMD => "MAC_CONTEXT", TIME_EVENT_CMD => "TIME_EVENT",
        TIME_EVENT_NOTIFICATION => "TIME_EVENT_NOTIF", BINDING_CONTEXT_CMD => "BINDING_CONTEXT",
        TIME_QUOTA_CMD => "TIME_QUOTA", LQ_CMD => "LQ", FW_PAGING_BLOCK_CMD => "FW_PAGING_BLOCK",
        PHY_CONFIGURATION_CMD => "PHY_CONFIGURATION", CALIB_RES_NOTIF_PHY_DB => "CALIB_RES_NOTIF_PHY_DB",
        PHY_DB_CMD => "PHY_DB", POWER_TABLE_CMD => "POWER_TABLE", NVM_ACCESS_CMD => "NVM_ACCESS",
        TX_ANT_CONFIGURATION_CMD => "TX_ANT_CONFIGURATION", BT_CONFIG => "BT_CONFIG",
        STATISTICS_NOTIFICATION => "STATISTICS", MISSED_BEACONS_NOTIFICATION => "MISSED_BEACONS",
        MAC_PM_POWER_TABLE => "MAC_PM_POWER_TABLE", SCAN_ITERATION_COMPLETE_UMAC => "SCAN_ITERATION_COMPLETE",
        REPLY_RX_PHY_CMD => "RX_PHY", REPLY_RX_MPDU_CMD => "RX_MPDU", BA_NOTIF => "BA_NOTIF",
        MCC_UPDATE_CMD => "MCC_UPDATE", MCC_CHUB_UPDATE_CMD => "MCC_CHUB_UPDATE", REPLY_SF_CFG_CMD => "SF_CFG",
        DTS_MEASUREMENT_NOTIFICATION => "DTS_MEASUREMENT", DEBUG_LOG_MSG => "DEBUG_LOG_MSG",
        MFUART_LOAD_NOTIFICATION => "MFUART_LOAD", LTR_CONFIG => "LTR_CONFIG", BEACON_NOTIFICATION => "BEACON_NOTIF",
        _ => "?",
    }
}

// ── little-endian helpers ───────────────────────────────────────────────────

pub fn le16(d: &[u8], o: usize) -> Option<u16> { d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]])) }
pub fn le32(d: &[u8], o: usize) -> Option<u32> { d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) }
pub fn le64(d: &[u8], o: usize) -> Option<u64> {
    d.get(o..o + 8).map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// Little-endian byte builder.
#[derive(Default)]
pub struct Buf(pub Vec<u8>);
impl Buf {
    pub fn new() -> Self { Buf(Vec::new()) }
    pub fn u8(&mut self, v: u8) -> &mut Self { self.0.push(v); self }
    pub fn u16(&mut self, v: u16) -> &mut Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    pub fn u32(&mut self, v: u32) -> &mut Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    pub fn u64(&mut self, v: u64) -> &mut Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self { self.0.extend_from_slice(b); self }
    pub fn zeros(&mut self, n: usize) -> &mut Self { self.0.resize(self.0.len() + n, 0); self }
    pub fn len(&self) -> usize { self.0.len() }
    pub fn put32(&mut self, off: usize, v: u32) { self.0[off..off + 4].copy_from_slice(&v.to_le_bytes()); }
    pub fn done(&mut self) -> Vec<u8> { core::mem::take(&mut self.0) }
}

// ── ALIVE ───────────────────────────────────────────────────────────────────

pub const ALIVE_STATUS_OK: u16 = 0xcafe;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Alive {
    pub status: u16,
    pub flags: u16,
    pub ucode_major: u32,
    pub ucode_minor: u32,
    pub ver_type: u8,
    pub ver_subtype: u8,
    pub lmac_error_table: u32,
    pub log_event_table: u32,
    pub scd_base: u32,
    pub umac_major: u32,
    pub umac_minor: u32,
    pub umac_error_table: u32,
    /// Number of LMACs in the notification (1 = v3, 2 = v4+).
    pub lmacs: u8,
}

const LMAC_ALIVE_LEN: usize = 48;
const UMAC_ALIVE_LEN: usize = 16;

/// UCODE_ALIVE_NTFY v3 (one LMAC, 68 bytes) or v4/v5 (two LMACs).
pub fn parse_alive(p: &[u8]) -> Option<Alive> {
    let lmacs = if p.len() == 4 + LMAC_ALIVE_LEN + UMAC_ALIVE_LEN {
        1
    } else if p.len() >= 4 + 2 * LMAC_ALIVE_LEN + UMAC_ALIVE_LEN {
        2
    } else {
        return None;
    };
    let l = 4;
    let u = 4 + lmacs * LMAC_ALIVE_LEN;
    Some(Alive {
        status: le16(p, 0)?,
        flags: le16(p, 2)?,
        ucode_major: le32(p, l)?,
        ucode_minor: le32(p, l + 4)?,
        ver_subtype: p[l + 8],
        ver_type: p[l + 9],
        lmac_error_table: le32(p, l + 16)?,
        log_event_table: le32(p, l + 20)?,
        scd_base: le32(p, l + 36)?,
        umac_major: le32(p, u)?,
        umac_minor: le32(p, u + 4)?,
        umac_error_table: le32(p, u + 8)? & !0xc000_0000,
        lmacs: lmacs as u8,
    })
}

// ── NVM ─────────────────────────────────────────────────────────────────────

pub const NVM_SECTION_SW: u16 = 1;
pub const NVM_SECTION_REGULATORY: u16 = 3;
pub const NVM_SECTION_CALIBRATION: u16 = 4;
pub const NVM_SECTION_PRODUCTION: u16 = 5;
pub const NVM_SECTION_HW_8000: u16 = 10;
pub const NVM_SECTION_MAC_OVERRIDE: u16 = 11;
pub const NVM_SECTION_PHY_SKU: u16 = 12;
pub const NVM_MAX_SECTIONS: u16 = 13;
pub const NVM_CHUNK: u16 = 2048;
pub const NVM_READ_STATUS_NOT_VALID_ADDRESS: u16 = 1;

/// NVM_ACCESS_CMD (v2) read request.
pub fn nvm_read_cmd(section: u16, offset: u16, length: u16) -> Vec<u8> {
    let mut b = Buf::new();
    b.u8(0).u8(0).u16(section).u16(offset).u16(length);
    b.done()
}

/// NVM_ACCESS response: (offset, length, type, status, data). As in Linux
/// (iwl_nvm_read_chunk) the status is judged before the length: a failed
/// read (e.g. a section the part does not have) carries no data, whatever
/// its length field says, so `data` is empty when status != 0.
pub fn parse_nvm_resp(p: &[u8]) -> Option<(u16, u16, u16, u16, &[u8])> {
    let off = le16(p, 0)?;
    let len = le16(p, 2)?;
    let ty = le16(p, 4)?;
    let status = le16(p, 6)?;
    let data = if status != 0 { &p[8..8] } else { p.get(8..8 + len as usize)? };
    Some((off, len, ty, status, data))
}

/// The family-8000 ("extended NVM") channel order.
pub const EXT_NVM_CHANNELS: [u8; 51] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14,
    36, 40, 44, 48, 52, 56, 60, 64, 68, 72, 76, 80, 84, 88, 92,
    96, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144,
    149, 153, 157, 161, 165, 169, 173, 177, 181,
];

pub mod chan_flags {
    pub const VALID: u16 = 1 << 0;
    pub const IBSS: u16 = 1 << 1;
    pub const ACTIVE: u16 = 1 << 3;
    pub const RADAR: u16 = 1 << 4;
    pub const INDOOR_ONLY: u16 = 1 << 5;
    pub const MHZ40: u16 = 1 << 9;
    pub const MHZ80: u16 = 1 << 10;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channel {
    pub ch: u8,
    pub flags: u16,
}

#[derive(Clone, Debug, Default)]
pub struct NvmData {
    pub nvm_version: u16,
    pub radio_cfg: u32,
    pub valid_tx_ant: u8,
    pub valid_rx_ant: u8,
    pub sku: u32,
    pub band_24: bool,
    pub band_52: bool,
    pub ht: bool,
    pub vht: bool,
    pub mimo_disabled: bool,
    pub n_hw_addrs: u16,
    pub lar_enabled: bool,
    /// MAC from the MAC_OVERRIDE section when valid and not the reserved
    /// placeholder 02:cc:aa:ff:ee:00 (else None: use the OTP registers).
    pub mac_override: Option<[u8; 6]>,
    /// Channels with the VALID flag, in NVM order.
    pub channels: Vec<Channel>,
}

pub fn valid_unicast_mac(m: &[u8; 6]) -> bool { m[0] & 1 == 0 && m.iter().any(|&b| b != 0) }

/// Parse the family-8000 sections (SW, REGULATORY, PHY_SKU and optional
/// MAC_OVERRIDE), like iwl_parse_nvm_data for IWL_NVM_EXT.
pub fn parse_nvm_8000(sw: &[u8], regulatory: &[u8], phy_sku: &[u8], mac_override: Option<&[u8]>) -> Option<NvmData> {
    let mut d = NvmData::default();
    d.nvm_version = le16(sw, 0)?;
    d.n_hw_addrs = le16(sw, 2 * 3).unwrap_or(0);
    d.radio_cfg = le32(phy_sku, 0)?;
    d.valid_tx_ant = ((d.radio_cfg >> 24) & 0xf) as u8;
    d.valid_rx_ant = ((d.radio_cfg >> 28) & 0xf) as u8;
    d.sku = le32(phy_sku, 2 * 2)?;
    d.band_24 = d.sku & 1 != 0;
    d.band_52 = d.sku & 2 != 0;
    d.ht = d.sku & 4 != 0;
    d.vht = d.ht && d.sku & 8 != 0;
    d.mimo_disabled = d.sku & 0x20 != 0;
    let lar_off = if d.nvm_version < 0xe39 { 0x4c7 } else { 0x507 };
    d.lar_enabled = le16(regulatory, 2 * lar_off).map_or(false, |v| v & 0x7 != 0);
    for (i, &ch) in EXT_NVM_CHANNELS.iter().enumerate() {
        let f = match le16(regulatory, 2 * i) { Some(f) => f, None => break };
        if f & chan_flags::VALID == 0 { continue; }
        if ch > 14 && !d.band_52 { continue; }
        if ch <= 14 && !d.band_24 { continue; }
        d.channels.push(Channel { ch, flags: f });
    }
    if let Some(mo) = mac_override {
        if let Some(b) = mo.get(2..8) {
            let m = [b[0], b[1], b[2], b[3], b[4], b[5]];
            if valid_unicast_mac(&m) && m != [0x02, 0xcc, 0xaa, 0xff, 0xee, 0x00] { d.mac_override = Some(m); }
        }
    }
    Some(d)
}

/// The MAC address from the WFMP_MAC_ADDR_0/1 OTP registers.
pub fn mac_from_otp(a0: u32, a1: u32) -> [u8; 6] {
    let b0 = a0.to_le_bytes();
    let b1 = a1.to_le_bytes();
    [b0[3], b0[2], b0[1], b0[0], b1[1], b1[0]]
}

// ── simple commands ─────────────────────────────────────────────────────────

pub fn tx_ant_cfg(valid: u8) -> Vec<u8> { (valid as u32).to_le_bytes().to_vec() }

/// PHY_CONFIGURATION_CMD v1: phy_cfg, then calib flow/event triggers.
pub fn phy_cfg_cmd(phy_cfg: u32, flow_trigger: u32, event_trigger: u32) -> Vec<u8> {
    let mut b = Buf::new();
    b.u32(phy_cfg).u32(flow_trigger).u32(event_trigger);
    b.done()
}

/// iwl_mvm_get_phy_config: the firmware PHY config with the chains masked
/// to the valid antennas.
pub fn phy_config(fw_phy_config: u32, tx_ant: u8, rx_ant: u8) -> u32 {
    let keep = !((0xf << 16) | (0xf << 20)) | ((tx_ant as u32) << 16) | ((rx_ant as u32) << 20);
    fw_phy_config & keep
}

pub fn phy_db_cmd(ty: u16, data: &[u8]) -> Vec<u8> {
    let mut b = Buf::new();
    b.u16(ty).u16(data.len() as u16).bytes(data);
    b.done()
}

/// CALIB_RES_NOTIF_PHY_DB: (type, data). Data of the per-channel-group
/// types starts with the group id (le16).
pub fn parse_phy_db(p: &[u8]) -> Option<(u16, &[u8])> {
    let ty = le16(p, 0)?;
    let len = le16(p, 2)? as usize;
    Some((ty, p.get(4..4 + len)?))
}

pub const PHY_DB_CFG: u16 = 1;
pub const PHY_DB_CALIB_NCH: u16 = 2;
pub const PHY_DB_CALIB_CHG_PAPD: u16 = 4;
pub const PHY_DB_CALIB_CHG_TXP: u16 = 5;

/// The calibration results of the INIT image, replayed to the runtime image
/// in Linux's order: CFG, NCH, then every PAPD group, then every TXP group.
#[derive(Default)]
pub struct PhyDb {
    pub entries: Vec<(u16, u16, Vec<u8>)>,
}
impl PhyDb {
    pub fn add(&mut self, ty: u16, data: &[u8]) {
        let grp = if ty == PHY_DB_CALIB_CHG_PAPD || ty == PHY_DB_CALIB_CHG_TXP { le16(data, 0).unwrap_or(0) } else { 0 };
        self.entries.retain(|e| !(e.0 == ty && e.1 == grp));
        self.entries.push((ty, grp, data.to_vec()));
    }
    pub fn ordered(&self) -> Vec<(u16, &[u8])> {
        let mut v: Vec<&(u16, u16, Vec<u8>)> = self.entries.iter().filter(|e| !e.2.is_empty()).collect();
        let rank = |t: u16| match t { PHY_DB_CFG => 0, PHY_DB_CALIB_NCH => 1, PHY_DB_CALIB_CHG_PAPD => 2, _ => 3 };
        v.sort_by_key(|e| (rank(e.0), e.1));
        v.into_iter().map(|e| (e.0, &e.2[..])).collect()
    }
    pub fn has_cfg(&self) -> bool { self.entries.iter().any(|e| e.0 == PHY_DB_CFG) }
}

/// BT_CONFIG: mode NW, SYNC2SCO | MPLUT (when supported) | HIGH_BAND_RET.
pub fn bt_config_cmd(mplut: bool) -> Vec<u8> {
    let mut b = Buf::new();
    b.u32(1).u32(0x4 | 0x10 | if mplut { 1 } else { 0 });
    b.done()
}

pub fn dqa_enable_cmd(cmd_queue: u32) -> Vec<u8> { cmd_queue.to_le_bytes().to_vec() }

/// POWER_TABLE_CMD (device power): flags; bit 0 = power save allowed.
pub fn device_power_cmd(power_save: bool) -> Vec<u8> {
    let mut b = Buf::new();
    b.u16(if power_save { 1 } else { 0 }).u16(0);
    b.done()
}

/// REPLY_SF_CFG_CMD (v2). `sta_watermark` = Some(watermark) for FULL_ON
/// with an associated AP (legacy 4096, SISO 6144, MIMO2 8192).
pub const SF_FULL_ON: u32 = 1;
pub const SF_UNINIT: u32 = 2;
pub const SF_INIT_OFF: u32 = 3;
pub fn sf_cfg_cmd(state: u32, sta_watermark: Option<u32>) -> Vec<u8> {
    let mut b = Buf::new();
    b.u32(state);
    b.u32(4096); // SF_LONG_DELAY_ON watermark (scan)
    b.u32(sta_watermark.unwrap_or(8192));
    for _ in 0..10 { b.u32(1_000_000); }
    let (aging, idle) = if sta_watermark.is_some() {
        ([2016u32, 2016, 10016, 2016, 2016], [320u32, 320, 2016, 320, 320])
    } else {
        ([400u32; 5], [160u32; 5])
    };
    for i in 0..5 { b.u32(aging[i]).u32(idle[i]); }
    b.done()
}

/// TEMP_REPORTING_THRESHOLDS_CMD with no thresholds (hands CT-kill and TX
/// backoff to the firmware).
pub fn temp_report_ths_cmd() -> Vec<u8> {
    let mut b = Buf::new();
    b.u32(0).zeros(8 * 2);
    b.done()
}

/// MCC_UPDATE_CMD (v2): request the current regulatory domain ("ZZ",
/// MCC_SOURCE_GET_CURRENT) or set one.
pub const MCC_SOURCE_OLD_FW: u8 = 0;
pub const MCC_SOURCE_GET_CURRENT: u8 = 0x10;
pub fn mcc_update_cmd(alpha2: [u8; 2], source: u8) -> Vec<u8> {
    let mut b = Buf::new();
    b.u16(((alpha2[0] as u16) << 8) | alpha2[1] as u16).u8(source).u8(0).u32(0).zeros(20);
    b.done()
}

#[derive(Clone, Debug, Default)]
pub struct MccResp {
    pub status: u32,
    pub mcc: [u8; 2],
    pub cap: u32,
    pub source: u8,
    /// Per-channel flags in EXT_NVM_CHANNELS order.
    pub channels: Vec<u32>,
}

/// MCC update response v3 (no 11ax capa) or v4 (11ax capa).
pub fn parse_mcc_resp(p: &[u8], v4: bool) -> Option<MccResp> {
    let status = le32(p, 0)?;
    let mcc = le16(p, 4)?;
    let (cap, source, n_off) = if v4 {
        (le16(p, 6)? as u32, *p.get(12)?, 16)
    } else {
        (*p.get(6)? as u32, *p.get(7)?, 12)
    };
    let n = le32(p, n_off)? as usize;
    if n > 64 { return None; }
    let mut channels = Vec::with_capacity(n);
    for i in 0..n { channels.push(le32(p, n_off + 4 + 4 * i)?); }
    Some(MccResp { status, mcc: [(mcc >> 8) as u8, mcc as u8], cap, source, channels })
}

/// FW_PAGING_BLOCK_CMD (long group): secured paging with `blocks` 32 KiB
/// blocks after the CSS page; `phys` = CSS page then the blocks.
pub fn paging_cmd(blocks: u32, pages_in_last: u32, phys: &[u64]) -> Vec<u8> {
    let mut b = Buf::new();
    b.u32((1 << 9) | (1 << 8) | pages_in_last).u32(15).u32(blocks);
    for i in 0..33 { b.u32(phys.get(i).map_or(0, |p| (p >> 12) as u32)); }
    b.done()
}

// ── contexts ────────────────────────────────────────────────────────────────

pub const FW_CTXT_ACTION_ADD: u32 = 1;
pub const FW_CTXT_ACTION_MODIFY: u32 = 2;
pub const FW_CTXT_ACTION_REMOVE: u32 = 3;
pub fn id_and_color(id: u32, color: u32) -> u32 { id | (color << 8) }

pub const PHY_BAND_5: u8 = 0;
pub const PHY_BAND_24: u8 = 1;

/// PHY_CONTEXT_CMD v1 with the 4-byte (non ultra-high-band) channel info:
/// 20 MHz on `channel`, `rx_ant`/`tx_ant` chains.
pub fn phy_ctxt_cmd(id: u32, color: u32, action: u32, channel: u8, tx_ant: u8, rx_ant: u8) -> Vec<u8> {
    let band = if channel <= 14 { PHY_BAND_24 } else { PHY_BAND_5 };
    let n_rx = if rx_ant.count_ones() >= 2 { 2u32 } else { 1 };
    let rxchain = ((rx_ant as u32) << 1) | (n_rx << 10) | (n_rx << 12);
    let mut b = Buf::new();
    b.u32(id_and_color(id, color)).u32(action).u32(0).u32(0);
    b.u8(band).u8(channel).u8(0).u8(0); // width 20 MHz, ctrl pos 0
    b.u32(tx_ant as u32).u32(rxchain).u32(0).u32(0);
    b.done()
}

// ── stations and queues ─────────────────────────────────────────────────────

pub const STA_LINK: u8 = 0;
pub const STA_AUX_ACTIVITY: u8 = 4;
pub const ADD_STA_SUCCESS: u32 = 1;
pub const STA_FLG_CLASS_AUTH: u32 = 1 << 14;
pub const STA_FLG_CLASS_ASSOC: u32 = 1 << 15;
pub const STA_MODE_ADD: u8 = 0;
pub const STA_MODE_MODIFY: u8 = 1;

pub struct AddSta {
    pub modify: bool,
    pub sta_id: u8,
    pub mac_id_n_color: u32,
    pub addr: [u8; 6],
    pub station_type: u8,
    pub tid_disable_tx: u16,
    pub modify_mask: u8,
    pub station_flags: u32,
    pub station_flags_msk: u32,
    pub assoc_id: u16,
    pub tfd_queue_msk: u32,
}

/// ADD_STA (v10, 48 bytes).
pub fn add_sta_cmd(s: &AddSta) -> Vec<u8> {
    let mut b = Buf::new();
    b.u8(if s.modify { STA_MODE_MODIFY } else { STA_MODE_ADD }).u8(0).u16(s.tid_disable_tx);
    b.u32(s.mac_id_n_color).bytes(&s.addr).u16(0);
    b.u8(s.sta_id).u8(s.modify_mask).u16(0);
    b.u32(s.station_flags).u32(s.station_flags_msk);
    b.u8(0).u8(0).u16(0); // immediate BA
    b.u16(0).u8(0).u8(s.station_type); // sleep_tx_count, sleep_state_flags, type
    b.u16(s.assoc_id).u16(0); // beamform flags
    b.u32(s.tfd_queue_msk);
    b.u16(0).u8(0).u8(0); // rx_ba_window, sp_length, uapsd_acs
    b.done()
}

/// SCD_QUEUE_CFG (v1): enable `queue` for (sta_id, tid) on `fifo`.
pub fn scd_queue_cfg_cmd(queue: u8, enable: bool, sta_id: u8, tid: u8, fifo: u8, ssn: u16) -> Vec<u8> {
    let mut b = Buf::new();
    b.u8(0).u8(sta_id).u8(tid).u8(queue).u8(enable as u8).u8(0).u8(fifo).u8(64).u16(ssn).u16(0);
    b.done()
}

pub const MAX_TID_COUNT: u8 = 8;
pub const TX_FIFO_BE: u8 = 1;
pub const TX_FIFO_VO: u8 = 3;
pub const TX_FIFO_MCAST: u8 = 5;
pub const DQA_AUX_QUEUE: u8 = 1;
pub const DQA_BSS_CLIENT_QUEUE: u8 = 4;
pub const MAC_INDEX_AUX: u32 = 4;

// ── scan ────────────────────────────────────────────────────────────────────

/// SCAN_CFG_CMD (legacy v1 layout + channel array): the per-device scan
/// configuration. `channels` are the channel numbers the device may scan.
pub fn scan_config_v1(tx_ant: u8, rx_ant: u8, mac: &[u8; 6], aux_sta: u8, channels: &[u8], n_scan_channels: usize) -> Vec<u8> {
    let n = channels.len().min(n_scan_channels);
    let flags: u32 = (1 << 0) | (1 << 3) | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (1 << 14) | (1 << 15)
        | (1 << 13) | (1 << 17) | ((n as u32) << 26);
    // All legacy rates, as both basic and supported.
    let rates: u32 = 0x0fff | (0x0fff << 16);
    let mut b = Buf::new();
    b.u32(flags).u32(tx_ant as u32).u32(rx_ant as u32).u32(rates);
    b.u32(0).u32(0); // out_of_channel_time, suspend_time (unassociated)
    b.u8(10).u8(110).u8(44).u8(90); // dwell: active, passive, fragmented, extended
    b.bytes(mac).u8(aux_sta).u8(0x1 | 0x2 | 0x4 | 0x8);
    let mut chans = [0u8; 256];
    chans[..n].copy_from_slice(&channels[..n]);
    b.bytes(&chans[..n_scan_channels]);
    b.done()
}

pub const SCAN_GEN_FLAGS_PASS_ALL: u16 = 1 << 2;
pub const SCAN_GEN_FLAGS_PASSIVE: u16 = 1 << 3;
pub const SCAN_GEN_FLAGS_PRE_CONNECT: u16 = 1 << 4;
pub const SCAN_GEN_FLAGS_ITER_COMPLETE: u16 = 1 << 5;
pub const SCAN_GEN_FLAGS_ADAPTIVE_DWELL: u16 = 1 << 13;

/// SCAN_REQ_UMAC v7 (adaptive dwell, single LMAC, legacy channel config
/// and probe template). `ssids` are probed for in addition to the wildcard.
pub fn scan_req_umac_v7(uid: u32, mac_id: u8, mac: &[u8; 6], channels: &[u8], n_scan_channels: usize,
                        ssids: &[&[u8]], passive: bool) -> Vec<u8> {
    let mut b = Buf::new();
    b.u32(0).u32(uid).u32(6); // flags, uid, ooc_priority EXT_6
    let mut gen = SCAN_GEN_FLAGS_PASS_ALL | SCAN_GEN_FLAGS_ADAPTIVE_DWELL;
    if passive { gen |= SCAN_GEN_FLAGS_PASSIVE; }
    b.u16(gen).u8(0).u8(mac_id);
    // v7: active, passive, fragmented dwell, adwell n_aps, n_aps_social, reserved, max budget
    b.u8(10).u8(110).u8(44).u8(2).u8(10).u8(0).u16(300);
    b.u32(0).u32(0).u32(0).u32(0); // max_out_time[2], suspend_time[2]
    b.u32(6); // scan_priority EXT_6
    let n = channels.len().min(n_scan_channels);
    b.u8(0).u8(n as u8).u16(0); // channel params: flags (no EBS), count
    // Probe SSID list: the wildcard first, then the configured ones.
    let mut list: Vec<&[u8]> = Vec::new();
    list.push(&[]);
    for s in ssids { if !s.is_empty() && s.len() <= 32 && list.len() < 20 { list.push(s); } }
    let bitmap: u32 = (1u32 << list.len()) - 1;
    for i in 0..n_scan_channels {
        if i < n { b.u32(if passive { 0 } else { bitmap }).u8(channels[i]).u8(1).u16(0); } else { b.zeros(8); }
    }
    // tail v1: schedule[2], delay, reserved
    b.u16(0).u8(1).u8(0).zeros(4).u16(0).u16(0);
    // probe request template v1: mac_header, band_data[2], common_data, buf[512]
    let mut buf = Buf::new();
    buf.u16(0x0040).u16(0).bytes(&[0xff; 6]).bytes(mac).bytes(&[0xff; 6]).u16(0);
    buf.u8(0).u8(0); // empty SSID IE
    let hdr_len = buf.len();
    let b24_off = buf.len();
    buf.bytes(&[1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 50, 4, 0x30, 0x48, 0x60, 0x6c]);
    let b24_len = buf.len() - b24_off;
    let b5_off = buf.len();
    buf.bytes(&[1, 8, 0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c]);
    let b5_len = buf.len() - b5_off;
    let common_off = buf.len();
    b.u16(0).u16(hdr_len as u16);
    b.u16(b24_off as u16).u16(b24_len as u16);
    b.u16(b5_off as u16).u16(b5_len as u16);
    b.u16(common_off as u16).u16(0);
    let mut pb = buf.done();
    pb.resize(512, 0);
    b.bytes(&pb);
    // direct_scan[20]: {id 0, len, ssid[32]}
    for i in 0..20 {
        match list.get(i) {
            Some(s) => { let mut e = [0u8; 34]; e[1] = s.len() as u8; e[2..2 + s.len()].copy_from_slice(s); b.bytes(&e); }
            None => { b.zeros(34); }
        }
    }
    b.done()
}

/// SCAN_COMPLETE_UMAC: (uid, last_iter, status, ebs_status).
pub fn parse_scan_complete(p: &[u8]) -> Option<(u32, u8, u8, u8)> {
    Some((le32(p, 0)?, *p.get(5)?, *p.get(6)?, *p.get(7)?))
}

// ── RX ──────────────────────────────────────────────────────────────────────

pub const RX_PHY_INFO_LEN: usize = 68;
pub const RX_RES_PHY_FLAGS_BAND_24: u16 = 1 << 0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhyInfo {
    /// Device (GP2) time at signal acquisition, microseconds.
    pub system_timestamp: u32,
    pub tsf: u64,
    pub phy_flags: u16,
    pub channel: u16,
    pub energy_a: u8,
    pub energy_b: u8,
    pub rate_n_flags: u32,
}

impl PhyInfo {
    /// Signal in dBm (the stronger chain), -128 when unknown.
    pub fn signal_dbm(&self) -> i8 {
        let a = if self.energy_a != 0 { -(self.energy_a as i16) } else { -128 };
        let b = if self.energy_b != 0 { -(self.energy_b as i16) } else { -128 };
        a.max(b).max(-128) as i8
    }
}

/// REPLY_RX_PHY_CMD (struct iwl_rx_phy_info).
pub fn parse_rx_phy(p: &[u8]) -> Option<PhyInfo> {
    if p.len() < RX_PHY_INFO_LEN { return None; }
    let e = le32(p, 28)?;
    Some(PhyInfo {
        system_timestamp: le32(p, 4)?,
        tsf: le64(p, 8)?,
        phy_flags: le16(p, 20)?,
        channel: le16(p, 22)?,
        energy_a: (e & 0xff) as u8,
        energy_b: ((e >> 8) & 0xff) as u8,
        rate_n_flags: le32(p, 56)?,
    })
}

pub const RX_MPDU_STATUS_CRC_OK: u32 = 1 << 0;
pub const RX_MPDU_STATUS_OVERRUN_OK: u32 = 1 << 1;
pub const RX_MPDU_STATUS_SRC_STA_FOUND: u32 = 1 << 2;
pub const RX_MPDU_STATUS_ICV_OK: u32 = 1 << 5;
pub const RX_MPDU_STATUS_MIC_OK: u32 = 1 << 6;
pub const RX_MPDU_STATUS_SEC_MSK: u32 = 7 << 8;
pub const RX_MPDU_STATUS_SEC_NONE: u32 = 0;
pub const RX_MPDU_STATUS_SEC_CCM: u32 = 2 << 8;
pub const RX_MPDU_STATUS_SEC_ERR: u32 = 7 << 8;

/// REPLY_RX_MPDU_CMD: (802.11 frame without FCS, rx status word).
pub fn parse_rx_mpdu(p: &[u8]) -> Option<(&[u8], u32)> {
    let len = le16(p, 0)? as usize;
    let frame = p.get(4..4 + len)?;
    let status = le32(p, 4 + len)?;
    Some((frame, status))
}

/// How many bytes of crypto header/trailer the firmware left in a frame:
/// (header, trailer), or None when the frame failed decryption.
pub fn rx_crypto_lengths(fc: u16, status: u32) -> Option<(usize, usize)> {
    if fc & 0x4000 == 0 { return Some((0, 0)); }
    match status & RX_MPDU_STATUS_SEC_MSK {
        RX_MPDU_STATUS_SEC_NONE | RX_MPDU_STATUS_SEC_ERR => None,
        RX_MPDU_STATUS_SEC_CCM => if status & RX_MPDU_STATUS_MIC_OK != 0 { Some((8, 8)) } else { None },
        _ => None,
    }
}
