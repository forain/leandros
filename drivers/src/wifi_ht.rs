//! 802.11 QoS (WMM), HT (802.11n) and Block Ack pieces of a station, shared
//! by the Intel driver (iwlwifi) and the simulated Wi-Fi (wifi_sim):
//!
//! * WMM (Wi-Fi Multimedia, the QoS subset APs speak): the WMM Information
//!   element of an association request and the AP's WMM Parameter element
//!   (per-AC EDCA: AIFSN, CWmin/CWmax, TXOP), converted to what the firmware
//!   takes.
//! * HT: our HT Capabilities element (what Linux iwlwifi advertises for the
//!   8265 at 20 MHz: iwl_init_ht_hw_capab), the AP's HT Capabilities and HT
//!   Operation elements.
//! * Block Ack (802.11-2016 10.24, 9.6.5): ADDBA request / response and
//!   DELBA action frames, the Block Ack Request control frame, and the
//!   receive reorder buffer of an RX Block Ack session. The reorder logic is
//!   mac80211's (net/mac80211/rx.c: ieee80211_sta_manage_reorder_buf,
//!   ieee80211_sta_reorder_release): the 8000-family firmware with its single
//!   RX queue does not reorder A-MPDU subframes (Linux leaves it to mac80211:
//!   iwlmvm sets SUPPORTS_REORDERING_BUFFER only with the multi-queue RX API
//!   of the 9000 family and later), so the host must.
//!
//! Pure computation over byte slices (host tested in drivers/iwlwifi-tests):
//! parsers bounds-check and return None on malformed input.

use alloc::vec::Vec;

use crate::wifi_frame as wf;

pub const IE_HT_CAPS: u8 = 45;
pub const IE_HT_OP: u8 = 61;
pub const IE_VENDOR: u8 = 221;

/// Vendor OUI + type of the WMM elements (Microsoft, type 2).
pub const WMM_OUI_TYPE: [u8; 4] = [0x00, 0x50, 0xf2, 0x02];
pub const WMM_SUBTYPE_INFO: u8 = 0;
pub const WMM_SUBTYPE_PARAM: u8 = 1;

// ── WMM ─────────────────────────────────────────────────────────────────────

/// EDCA parameters of one access category, as the AP announces them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcParams {
    pub aifsn: u8,
    /// Admission control mandatory (we never request a TSPEC).
    pub acm: bool,
    pub cw_min: u16,
    pub cw_max: u16,
    /// TXOP limit in 32 us units (0 = one frame per access).
    pub txop: u16,
}

/// Access categories by ACI (the order of the WMM Parameter element).
pub const ACI_BE: usize = 0;
pub const ACI_BK: usize = 1;
pub const ACI_VI: usize = 2;
pub const ACI_VO: usize = 3;

/// The AP's WMM Parameter element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WmmParams {
    /// QoS Info: bits 0-3 the parameter set count, bit 7 U-APSD support.
    pub qos_info: u8,
    /// Indexed by ACI: BE, BK, VI, VO.
    pub ac: [AcParams; 4],
}

impl WmmParams {
    /// The 802.11 defaults for a BSS (EDCA table 9-137, dot11 values for a
    /// non-AP station): what an AP with only a WMM Information element (no
    /// parameters) implies.
    pub fn defaults() -> WmmParams {
        let ac = |aifsn, cw_min, cw_max, txop| AcParams { aifsn, acm: false, cw_min, cw_max, txop };
        WmmParams { qos_info: 0, ac: [ac(3, 15, 1023, 0), ac(7, 15, 1023, 0), ac(2, 7, 15, 94), ac(2, 3, 7, 47)] }
    }

    /// Parameter set count (changes when the AP changes its EDCA).
    pub fn set_count(&self) -> u8 { self.qos_info & 0x0f }
}

/// What the AP's WMM element is: absent, an Information element (WMM
/// without parameters), or the full Parameter element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wmm {
    Info(u8),
    Param(WmmParams),
}

impl Wmm {
    pub fn params(&self) -> WmmParams {
        match *self { Wmm::Param(p) => p, Wmm::Info(q) => WmmParams { qos_info: q, ..WmmParams::defaults() } }
    }
}

/// The WMM element (Information or Parameter) among `ies`, version 1 only.
pub fn find_wmm(ies: &[u8]) -> Option<Wmm> {
    for (id, d) in wf::ies(ies) {
        if id != IE_VENDOR || d.len() < 7 || d[0..4] != WMM_OUI_TYPE || d[5] != 1 { continue; }
        match d[4] {
            WMM_SUBTYPE_INFO => return Some(Wmm::Info(d[6])),
            WMM_SUBTYPE_PARAM => if let Some(p) = parse_wmm_param(d) { return Some(Wmm::Param(p)); },
            _ => {}
        }
    }
    None
}

/// The body of a WMM Parameter element (OUI, type, subtype, version, QoS
/// info, reserved, four AC records). Records are placed by their ACI field;
/// an invalid one (AIFSN below 2, which a non-AP station may not use, or
/// CWmax below CWmin) keeps the default, as mac80211 does.
pub fn parse_wmm_param(d: &[u8]) -> Option<WmmParams> {
    if d.len() < 24 || d[0..4] != WMM_OUI_TYPE || d[4] != WMM_SUBTYPE_PARAM || d[5] != 1 { return None; }
    let mut p = WmmParams::defaults();
    p.qos_info = d[6];
    for i in 0..4 {
        let r = &d[8 + 4 * i..12 + 4 * i];
        let aci = ((r[0] >> 5) & 3) as usize;
        let aifsn = r[0] & 0x0f;
        let cw_min = (1u16 << (r[1] & 0x0f)) - 1;
        let cw_max = (1u16 << (r[1] >> 4)) - 1;
        let txop = u16::from_le_bytes([r[2], r[3]]);
        if aifsn < 2 || cw_max < cw_min { continue; }
        p.ac[aci] = AcParams { aifsn, acm: r[0] & 0x10 != 0, cw_min, cw_max, txop };
    }
    Some(p)
}

/// A WMM Information element for an association request (`qos_info` 0: no
/// U-APSD).
pub fn wmm_info_ie(qos_info: u8) -> [u8; 9] {
    [IE_VENDOR, 7, 0x00, 0x50, 0xf2, 0x02, WMM_SUBTYPE_INFO, 1, qos_info]
}

/// The firmware's EDCA parameters (MAC_CONTEXT `ac[]`), in the firmware's
/// AC order BK, BE, VI, VO: (cw_min, cw_max, aifsn, FIFO, TXOP in us), as
/// iwl_mvm_set_fw_qos_params (txop * 32; FIFO BK 0, BE 1, VI 2, VO 3).
pub fn fw_edca(p: &WmmParams) -> [(u16, u16, u8, u8, u16); 4] {
    let one = |aci: usize, fifo: u8| {
        let a = p.ac[aci];
        (a.cw_min, a.cw_max, a.aifsn, fifo, a.txop.saturating_mul(32))
    };
    [one(ACI_BK, 0), one(ACI_BE, 1), one(ACI_VI, 2), one(ACI_VO, 3)]
}

/// 802.1D user priority (TID 0-7) to WMM access category (ACI).
pub fn tid_to_aci(tid: u8) -> usize {
    match tid & 7 { 1 | 2 => ACI_BK, 0 | 3 => ACI_BE, 4 | 5 => ACI_VI, _ => ACI_VO }
}

// ── HT ──────────────────────────────────────────────────────────────────────

/// HT Capabilities Information bits (9.4.2.56.2).
pub mod htcap {
    pub const LDPC: u16 = 0x0001;
    pub const SUP_WIDTH_20_40: u16 = 0x0002;
    pub const SM_PS_MASK: u16 = 0x000c;
    pub const SM_PS_SHIFT: u16 = 2;
    pub const GRN_FLD: u16 = 0x0010;
    pub const SGI_20: u16 = 0x0020;
    pub const SGI_40: u16 = 0x0040;
    pub const TX_STBC: u16 = 0x0080;
    pub const RX_STBC_MASK: u16 = 0x0300;
    pub const RX_STBC_SHIFT: u16 = 8;
    pub const DELAY_BA: u16 = 0x0400;
    pub const MAX_AMSDU: u16 = 0x0800;
    pub const DSSSCCK40: u16 = 0x1000;
    pub const INTOLERANT_40: u16 = 0x4000;
    pub const LSIG_TXOP: u16 = 0x8000;
}

/// SM Power Save modes (HT Capabilities bits 2-3).
pub const SM_PS_STATIC: u8 = 0;
pub const SM_PS_DYNAMIC: u8 = 1;
pub const SM_PS_DISABLED: u8 = 3;

/// An HT Capabilities element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HtCap {
    pub cap: u16,
    /// Maximum A-MPDU length exponent (0..3: 8K..64K).
    pub ampdu_factor: u8,
    /// Minimum MPDU start spacing (0..7).
    pub ampdu_density: u8,
    /// Supported RX MCS bitmap (MCS 0-76).
    pub rx_mask: [u8; 10],
    pub rx_highest: u16,
    pub tx_params: u8,
}

impl HtCap {
    /// Spatial streams the station receives (rx_mask bytes 0-3 in use).
    pub fn rx_streams(&self) -> u8 {
        self.rx_mask[..4].iter().take_while(|&&b| b != 0).count() as u8
    }
    pub fn sm_ps(&self) -> u8 { ((self.cap & htcap::SM_PS_MASK) >> htcap::SM_PS_SHIFT) as u8 }
    pub fn sgi20(&self) -> bool { self.cap & htcap::SGI_20 != 0 }
    /// The station can receive LDPC-coded frames.
    pub fn ldpc(&self) -> bool { self.cap & htcap::LDPC != 0 }
    /// Receives single-stream STBC.
    pub fn rx_stbc(&self) -> bool { self.cap & htcap::RX_STBC_MASK != 0 }
}

pub fn parse_ht_cap(d: &[u8]) -> Option<HtCap> {
    if d.len() < 26 { return None; }
    let mut rx_mask = [0u8; 10];
    rx_mask.copy_from_slice(&d[3..13]);
    Some(HtCap {
        cap: u16::from_le_bytes([d[0], d[1]]),
        ampdu_factor: d[2] & 3,
        ampdu_density: (d[2] >> 2) & 7,
        rx_mask,
        rx_highest: u16::from_le_bytes([d[13], d[14]]) & 0x3ff,
        tx_params: d[15],
    })
}

/// An HT Operation element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HtOp {
    pub primary: u8,
    /// Bits 0-1 secondary channel offset, bit 2 STA channel width.
    pub ht_param: u8,
    /// Bits 0-1 HT protection, bit 2 non-greenfield STAs present, bit 4
    /// OBSS non-HT STAs present.
    pub op_mode: u16,
}

pub const HT_PROT_NONE: u8 = 0;
pub const HT_PROT_NONMEMBER: u8 = 1;
pub const HT_PROT_20MHZ: u8 = 2;
pub const HT_PROT_NONHT_MIXED: u8 = 3;

impl HtOp {
    pub fn protection(&self) -> u8 { (self.op_mode & 3) as u8 }
    /// HT protection the firmware must apply on a 20 MHz link
    /// (iwl_mvm_set_fw_protection_flags: non-member and mixed; the 20 MHz
    /// mode only protects wider transmissions).
    pub fn needs_ht_prot_20(&self) -> bool {
        matches!(self.protection(), HT_PROT_NONMEMBER | HT_PROT_NONHT_MIXED)
    }
}

pub fn parse_ht_op(d: &[u8]) -> Option<HtOp> {
    if d.len() < 22 { return None; }
    Some(HtOp { primary: d[0], ht_param: d[1], op_mode: u16::from_le_bytes([d[2], d[3]]) })
}

/// What a BSS's elements say about QoS and HT.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BssHt {
    pub wmm: Option<Wmm>,
    pub ht_cap: Option<HtCap>,
    pub ht_op: Option<HtOp>,
}

pub fn parse_bss_ies(ies: &[u8]) -> BssHt {
    let mut b = BssHt { wmm: find_wmm(ies), ..BssHt::default() };
    for (id, d) in wf::ies(ies) {
        match id {
            IE_HT_CAPS if b.ht_cap.is_none() => b.ht_cap = parse_ht_cap(d),
            IE_HT_OP if b.ht_op.is_none() => b.ht_op = parse_ht_op(d),
            _ => {}
        }
    }
    b
}

/// What we advertise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HtCfg {
    /// Receive chains (1 or 2): MCS 0-7, or 0-15.
    pub rx_streams: u8,
    /// Transmit chains (TX STBC needs two).
    pub tx_streams: u8,
    pub ldpc: bool,
    pub stbc: bool,
    pub sgi20: bool,
    /// SM power save mode (SM_PS_*).
    pub sm_ps: u8,
}

/// Linux iwlwifi for the 8265 (iwl8000_ht_params: STBC, LDPC; no
/// greenfield; 4K A-MSDU) restricted to 20 MHz: MCS 0-15 on two chains.
pub fn iwl8265_ht_cfg(tx_ant: u8, rx_ant: u8) -> HtCfg {
    HtCfg { rx_streams: (rx_ant & 3).count_ones().max(1) as u8, tx_streams: (tx_ant & 3).count_ones().max(1) as u8,
            ldpc: true, stbc: true, sgi20: true, sm_ps: SM_PS_DISABLED }
}

/// Our HT Capabilities element (id, length 26, body): 20 MHz only, the
/// 64K A-MPDU exponent and 4 us MPDU density Linux uses, the RX MCS set
/// for `rx_streams`, the highest RX rate (72 Mb/s per stream at 20 MHz).
pub fn ht_cap_ie(c: &HtCfg) -> [u8; 28] {
    let mut cap = (c.sm_ps as u16 & 3) << htcap::SM_PS_SHIFT;
    if c.ldpc { cap |= htcap::LDPC; }
    if c.sgi20 { cap |= htcap::SGI_20; }
    if c.stbc {
        cap |= 1 << htcap::RX_STBC_SHIFT;
        if c.tx_streams > 1 { cap |= htcap::TX_STBC; }
    }
    let streams = c.rx_streams.clamp(1, 2);
    let mut ie = [0u8; 28];
    ie[0] = IE_HT_CAPS;
    ie[1] = 26;
    ie[2..4].copy_from_slice(&cap.to_le_bytes());
    ie[4] = 3 | (5 << 2); // IEEE80211_HT_MAX_AMPDU_64K, IEEE80211_HT_MPDU_DENSITY_4
    for s in 0..streams as usize { ie[5 + s] = 0xff; }
    ie[15..17].copy_from_slice(&(72u16 * streams as u16).to_le_bytes());
    let mut tx = 0x01u8; // TX MCS set defined
    if c.tx_streams.clamp(1, 2) != streams {
        tx |= 0x02 | ((c.tx_streams.clamp(1, 2) - 1) << 2);
    }
    ie[17] = tx;
    ie
}

// ── Block Ack action frames ─────────────────────────────────────────────────

pub const CATEGORY_BACK: u8 = 3;
pub const ACTION_ADDBA_REQ: u8 = 0;
pub const ACTION_ADDBA_RESP: u8 = 1;
pub const ACTION_DELBA: u8 = 2;

/// Status codes used in ADDBA responses.
pub const STATUS_SUCCESS: u16 = 0;
pub const STATUS_REQUEST_DECLINED: u16 = 37;
pub const STATUS_INVALID_QOS_PARAM: u16 = 38;

/// DELBA reason: the session ends (QSTA_LEAVING 36, END_BA 37, TIMEOUT 39).
pub const REASON_END_BA: u16 = 37;

/// Largest reorder window we accept (IEEE80211_MAX_AMPDU_BUF_HT).
pub const MAX_BA_BUF: u16 = 64;

/// Block Ack Parameter Set (9.4.1.14): A-MSDU, policy, TID, buffer size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaParams {
    pub amsdu: bool,
    /// Immediate Block Ack (delayed is not supported).
    pub immediate: bool,
    pub tid: u8,
    pub buf_size: u16,
}

impl BaParams {
    pub fn from_u16(v: u16) -> BaParams {
        BaParams { amsdu: v & 1 != 0, immediate: v & 2 != 0, tid: ((v >> 2) & 0xf) as u8, buf_size: v >> 6 }
    }
    pub fn to_u16(&self) -> u16 {
        (self.amsdu as u16) | ((self.immediate as u16) << 1) | ((self.tid as u16 & 0xf) << 2) | ((self.buf_size & 0x3ff) << 6)
    }
}

/// A Block Ack action frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackAction {
    AddbaReq { dialog: u8, params: BaParams, timeout: u16, ssn: u16 },
    AddbaResp { dialog: u8, status: u16, params: BaParams, timeout: u16 },
    /// `initiator`: the sender is the session's originator (the AP for an
    /// RX session of ours).
    Delba { tid: u8, initiator: bool, reason: u16 },
}

/// A Block Ack action frame (management, subtype Action, category 3) from
/// the frame bytes (header included).
pub fn parse_back_action(frame: &[u8]) -> Option<BackAction> {
    let fc = wf::frame_control(frame)?;
    if !wf::is_mgmt(fc) || wf::subtype(fc) != wf::fc::STYPE_ACTION { return None; }
    let b = frame.get(24..)?;
    if *b.first()? != CATEGORY_BACK { return None; }
    let rd = |o: usize| -> Option<u16> { Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?])) };
    match *b.get(1)? {
        ACTION_ADDBA_REQ => Some(BackAction::AddbaReq {
            dialog: *b.get(2)?, params: BaParams::from_u16(rd(3)?), timeout: rd(5)?, ssn: rd(7)? >> 4,
        }),
        ACTION_ADDBA_RESP => Some(BackAction::AddbaResp {
            dialog: *b.get(2)?, status: rd(3)?, params: BaParams::from_u16(rd(5)?), timeout: rd(7)?,
        }),
        ACTION_DELBA => {
            let p = rd(2)?;
            Some(BackAction::Delba { tid: (p >> 12) as u8, initiator: p & 0x0800 != 0, reason: rd(4)? })
        }
        _ => None,
    }
}

fn action_header(da: &[u8; 6], sa: &[u8; 6], bssid: &[u8; 6]) -> Vec<u8> {
    let mut v = Vec::with_capacity(36);
    v.extend_from_slice(&wf::fc::STYPE_ACTION.to_le_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(da);
    v.extend_from_slice(sa);
    v.extend_from_slice(bssid);
    v.extend_from_slice(&[0, 0]);
    v
}

pub fn build_addba_req(da: &[u8; 6], sa: &[u8; 6], bssid: &[u8; 6], dialog: u8, params: BaParams, timeout: u16, ssn: u16) -> Vec<u8> {
    let mut v = action_header(da, sa, bssid);
    v.extend_from_slice(&[CATEGORY_BACK, ACTION_ADDBA_REQ, dialog]);
    v.extend_from_slice(&params.to_u16().to_le_bytes());
    v.extend_from_slice(&timeout.to_le_bytes());
    v.extend_from_slice(&((ssn & 0xfff) << 4).to_le_bytes());
    v
}

pub fn build_addba_resp(da: &[u8; 6], sa: &[u8; 6], bssid: &[u8; 6], dialog: u8, status: u16, params: BaParams, timeout: u16) -> Vec<u8> {
    let mut v = action_header(da, sa, bssid);
    v.extend_from_slice(&[CATEGORY_BACK, ACTION_ADDBA_RESP, dialog]);
    v.extend_from_slice(&status.to_le_bytes());
    v.extend_from_slice(&params.to_u16().to_le_bytes());
    v.extend_from_slice(&timeout.to_le_bytes());
    v
}

pub fn build_delba(da: &[u8; 6], sa: &[u8; 6], bssid: &[u8; 6], tid: u8, initiator: bool, reason: u16) -> Vec<u8> {
    let mut v = action_header(da, sa, bssid);
    v.extend_from_slice(&[CATEGORY_BACK, ACTION_DELBA]);
    let p = ((tid as u16 & 0xf) << 12) | if initiator { 0x0800 } else { 0 };
    v.extend_from_slice(&p.to_le_bytes());
    v.extend_from_slice(&reason.to_le_bytes());
    v
}

/// The answer to an ADDBA request, as mac80211's
/// ___ieee80211_start_rx_ba_session decides it: immediate policy only, a
/// TID 0-7, the window clamped to `max_buf` (0 in the request = the
/// maximum); `enabled` false declines everything. Returns the status and
/// the parameters for the response (A-MSDU echoed when `amsdu_ok`).
pub fn answer_addba(req: &BaParams, enabled: bool, max_buf: u16, amsdu_ok: bool) -> (u16, BaParams) {
    let buf = if req.buf_size == 0 { max_buf } else { req.buf_size.min(max_buf) };
    let params = BaParams { amsdu: req.amsdu && amsdu_ok, immediate: true, tid: req.tid, buf_size: buf };
    if !enabled { return (STATUS_REQUEST_DECLINED, BaParams { buf_size: 0, ..params }); }
    if !req.immediate || req.tid > 7 || buf == 0 { return (STATUS_INVALID_QOS_PARAM, BaParams { buf_size: 0, ..params }); }
    (STATUS_SUCCESS, params)
}

// ── Block Ack Request ───────────────────────────────────────────────────────

/// Frame Control of a Block Ack Request (control, subtype 8).
pub const FC_BAR: u16 = 0x0084;

/// A Block Ack Request: (TA, TID, starting sequence number). Basic and
/// compressed BARs only (no multi-TID).
pub fn parse_bar(frame: &[u8]) -> Option<([u8; 6], u8, u16)> {
    let fc = wf::frame_control(frame)?;
    if fc & 0x00fc != FC_BAR || frame.len() < 20 { return None; }
    let ctl = u16::from_le_bytes([frame[16], frame[17]]);
    if ctl & 0x0002 != 0 { return None; } // multi-TID
    let mut ta = [0u8; 6];
    ta.copy_from_slice(&frame[10..16]);
    Some((ta, (ctl >> 12) as u8, u16::from_le_bytes([frame[18], frame[19]]) >> 4))
}

/// The receiver address of a control frame.
pub fn ctl_ra(frame: &[u8]) -> Option<[u8; 6]> { wf::addr1(frame) }

pub fn build_bar(ra: &[u8; 6], ta: &[u8; 6], tid: u8, ssn: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(20);
    v.extend_from_slice(&FC_BAR.to_le_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(ra);
    v.extend_from_slice(ta);
    v.extend_from_slice(&((((tid as u16) & 0xf) << 12) | 0x0004).to_le_bytes()); // compressed bitmap
    v.extend_from_slice(&((ssn & 0xfff) << 4).to_le_bytes());
    v
}

/// QoS data frame from the AP to `da` (FromDS, unprotected) carrying the
/// Ethernet frame `eth`: the downlink frames of the simulated AP and of the
/// tests. `seq` is the 12-bit sequence number.
pub fn build_qos_data_from_ap(eth: &[u8], bssid: &[u8; 6], tid: u8, seq: u16, retry: bool) -> Option<Vec<u8>> {
    if eth.len() < 14 { return None; }
    let mut v = Vec::with_capacity(eth.len() + 26 + 8);
    let mut fc = wf::fc::STYPE_QOS_DATA | wf::fc::FROM_DS;
    if retry { fc |= wf::fc::RETRY; }
    v.extend_from_slice(&fc.to_le_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(&eth[0..6]); // DA
    v.extend_from_slice(bssid);
    v.extend_from_slice(&eth[6..12]); // SA
    v.extend_from_slice(&((seq & 0xfff) << 4).to_le_bytes());
    v.extend_from_slice(&[tid & 0xf, 0]);
    let et = u16::from_be_bytes([eth[12], eth[13]]);
    v.extend_from_slice(if et == 0x80f3 || et == 0x8137 { &[0xaa, 0xaa, 0x03, 0x00, 0x00, 0xf8] } else { &[0xaa, 0xaa, 0x03, 0x00, 0x00, 0x00] });
    v.extend_from_slice(&eth[12..]);
    Some(v)
}

/// TID and 12-bit sequence number of a QoS data frame, None for others
/// (non-QoS data, QoS Null, group addressed frames are never reordered).
pub fn qos_tid_seq(frame: &[u8]) -> Option<(u8, u16)> {
    let fc = wf::frame_control(frame)?;
    if !wf::is_data(fc) || fc & 0x0080 == 0 || fc & 0x0040 != 0 { return None; }
    if wf::addr1(frame)?[0] & 1 != 0 { return None; }
    let qos_off = if fc & (wf::fc::TO_DS | wf::fc::FROM_DS) == (wf::fc::TO_DS | wf::fc::FROM_DS) { 30 } else { 24 };
    let qc = *frame.get(qos_off)?;
    // Ack policy "no ack" (bits 5-6 = 01) is never part of a BA session.
    if (qc >> 5) & 3 == 1 { return None; }
    Some((qc & 0x0f, wf::seq_ctrl(frame)? >> 4))
}

// ── RX reorder buffer ───────────────────────────────────────────────────────

/// A frame waits at most this long for a missing earlier one
/// (mac80211's HT_RX_REORDER_BUF_TIMEOUT, HZ / 10).
pub const REORDER_TIMEOUT_MS: u64 = 100;

const SN_MASK: u16 = 0xfff;

/// Sequence-number arithmetic modulo 4096 (ieee80211_sn_less & co).
pub fn sn_less(a: u16, b: u16) -> bool { (a.wrapping_sub(b) & SN_MASK) > (SN_MASK + 1) / 2 }
pub fn sn_add(a: u16, b: u16) -> u16 { a.wrapping_add(b) & SN_MASK }
pub fn sn_sub(a: u16, b: u16) -> u16 { a.wrapping_sub(b) & SN_MASK }

/// What happened to one frame given to [`Reorder::rx`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// In order with nothing waiting: handed straight out.
    Direct,
    /// Held for an earlier missing frame (may have released others).
    Buffered,
    /// Older than the window: a retransmission of something already
    /// delivered (or given up on). Dropped.
    Old,
    /// Already waiting in the buffer. Dropped.
    Dup,
    /// Before the session's starting sequence number, before any frame of
    /// the session arrived: passed through unordered (mac80211 "!started").
    PreSession,
}

/// Counters of one session (or summed over sessions).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReorderStats {
    pub direct: u64,
    pub buffered: u64,
    pub old: u64,
    pub dup: u64,
    /// Frames released after waiting.
    pub released: u64,
    /// Sequence numbers given up on (holes skipped by timeout, window
    /// moves or a BAR).
    pub skipped: u64,
    /// Releases forced by the timeout.
    pub timeouts: u64,
    pub bars: u64,
    /// Most frames waiting at once.
    pub max_stored: u64,
}

impl ReorderStats {
    pub fn add(&mut self, o: &ReorderStats) {
        self.direct += o.direct;
        self.buffered += o.buffered;
        self.old += o.old;
        self.dup += o.dup;
        self.released += o.released;
        self.skipped += o.skipped;
        self.timeouts += o.timeouts;
        self.bars += o.bars;
        self.max_stored = self.max_stored.max(o.max_stored);
    }
}

/// The reorder buffer of one RX Block Ack session (one TID).
pub struct Reorder<T> {
    head: u16,
    buf_size: u16,
    slots: Vec<Option<(T, u64)>>,
    stored: usize,
    started: bool,
    pub stats: ReorderStats,
}

impl<T> Reorder<T> {
    /// A session starting at sequence number `ssn` with a window of
    /// `buf_size` (1..=64) frames.
    pub fn new(ssn: u16, buf_size: u16) -> Reorder<T> {
        let n = buf_size.clamp(1, MAX_BA_BUF) as usize;
        let mut slots = Vec::with_capacity(n);
        // Slots: a power of two (it divides 4096), so a window that wraps the
        // sequence space never maps two of its numbers to one slot.
        for _ in 0..n.next_power_of_two() { slots.push(None); }
        Reorder { head: ssn & SN_MASK, buf_size: n as u16, slots, stored: 0, started: false, stats: ReorderStats::default() }
    }

    pub fn head(&self) -> u16 { self.head }
    pub fn buf_size(&self) -> u16 { self.buf_size }
    pub fn stored(&self) -> usize { self.stored }

    fn idx(&self, sn: u16) -> usize { sn as usize % self.slots.len() }

    /// ieee80211_release_reorder_frame: hand out the head slot (if full),
    /// advance the head by one.
    fn release_head(&mut self, out: &mut Vec<T>) {
        let i = self.idx(self.head);
        match self.slots[i].take() {
            Some((t, _)) => { self.stored -= 1; self.stats.released += 1; out.push(t); }
            None => self.stats.skipped += 1,
        }
        self.head = sn_add(self.head, 1);
    }

    /// Release everything before `new_head` (ieee80211_release_reorder_frames).
    fn release_until(&mut self, new_head: u16, out: &mut Vec<T>) {
        while sn_less(self.head, new_head) { self.release_head(out); }
    }

    /// One MPDU of the session's TID with sequence number `sn` (received at
    /// `now` ms). In-order frames, and any waiting frames they unblock, are
    /// appended to `out` in sequence order.
    pub fn rx(&mut self, sn: u16, item: T, now: u64, out: &mut Vec<T>) -> Fate {
        let sn = sn & SN_MASK;
        if !self.started {
            if sn_less(sn, self.head) { out.push(item); return Fate::PreSession; }
            self.started = true;
        }
        if sn_less(sn, self.head) { self.stats.old += 1; return Fate::Old; }
        if !sn_less(sn, sn_add(self.head, self.buf_size)) {
            // Beyond the window: move it so this frame is its last slot.
            let new_head = sn_add(sn_sub(sn, self.buf_size), 1);
            self.release_until(new_head, out);
        }
        let i = self.idx(sn);
        if self.slots[i].is_some() { self.stats.dup += 1; return Fate::Dup; }
        if sn == self.head && self.stored == 0 {
            self.head = sn_add(self.head, 1);
            self.stats.direct += 1;
            out.push(item);
            return Fate::Direct;
        }
        self.slots[i] = Some((item, now));
        self.stored += 1;
        self.stats.buffered += 1;
        self.stats.max_stored = self.stats.max_stored.max(self.stored as u64);
        self.release(now, out);
        Fate::Buffered
    }

    /// The cases of [`rx`] that need no buffering, decided without the
    /// frame (so the caller copies only frames that must wait): `Direct`
    /// and `PreSession` are to be delivered now, `Old` and `Dup` dropped
    /// (bookkeeping done); None: call [`rx`] with the frame.
    pub fn fast(&mut self, sn: u16) -> Option<Fate> {
        let sn = sn & SN_MASK;
        if !self.started && sn_less(sn, self.head) { return Some(Fate::PreSession); }
        if self.started && sn_less(sn, self.head) { self.stats.old += 1; return Some(Fate::Old); }
        if sn == self.head && self.stored == 0 {
            self.started = true;
            self.head = sn_add(self.head, 1);
            self.stats.direct += 1;
            return Some(Fate::Direct);
        }
        if sn_less(sn, sn_add(self.head, self.buf_size)) && self.slots[self.idx(sn)].is_some() {
            self.stats.dup += 1;
            return Some(Fate::Dup);
        }
        None
    }

    /// ieee80211_sta_reorder_release: release in-order frames from the head;
    /// when the head is missing, give up on holes in front of frames that
    /// waited longer than REORDER_TIMEOUT_MS.
    pub fn release(&mut self, now: u64, out: &mut Vec<T>) {
        let index = self.idx(self.head);
        if self.slots[index].is_none() && self.stored > 0 {
            let n = self.slots.len();
            let mut skipped: u16 = 1;
            let mut j = (index + 1) % n;
            while j != index {
                let t = match &self.slots[j] { None => { skipped += 1; j = (j + 1) % n; continue; } Some((_, t)) => *t };
                if skipped > 0 && now <= t + REORDER_TIMEOUT_MS { return; }
                if skipped > 0 { self.stats.timeouts += 1; self.stats.skipped += skipped as u64; }
                // Release slot j (its sequence number is head + skipped).
                let (item, _) = self.slots[j].take().unwrap();
                self.stored -= 1;
                self.stats.released += 1;
                out.push(item);
                self.head = sn_add(self.head, skipped + 1);
                skipped = 0;
                j = (j + 1) % n;
            }
        } else {
            while self.slots[self.idx(self.head)].is_some() { self.release_head(out); }
        }
    }

    /// A Block Ack Request with starting sequence number `ssn`: the
    /// originator gave up on everything before it.
    pub fn bar(&mut self, ssn: u16, now: u64, out: &mut Vec<T>) {
        self.stats.bars += 1;
        self.started = true;
        self.release_until(ssn & SN_MASK, out);
        self.release(now, out);
    }

    /// The session ends: every waiting frame, in order.
    pub fn flush(&mut self, out: &mut Vec<T>) {
        let mut left = self.stored;
        while left > 0 {
            let i = self.idx(self.head);
            if self.slots[i].is_some() { left -= 1; }
            self.release_head(out);
        }
    }

    /// The oldest waiting frame's arrival time (when a timeout check is due).
    pub fn oldest(&self) -> Option<u64> { self.slots.iter().filter_map(|s| s.as_ref().map(|x| x.1)).min() }
}

/// The RX Block Ack sessions of one peer, by TID.
pub struct RxAgg<T> {
    pub tids: [Option<Reorder<T>>; 8],
    /// Counters of the sessions that ended (current ones add their own).
    pub ended: ReorderStats,
    pub sessions: u64,
    pub declined: u64,
}

impl<T> Default for RxAgg<T> {
    fn default() -> Self { RxAgg { tids: Default::default(), ended: ReorderStats::default(), sessions: 0, declined: 0 } }
}

impl<T> RxAgg<T> {
    pub fn active(&self) -> usize { self.tids.iter().filter(|t| t.is_some()).count() }
    pub fn has(&self, tid: u8) -> bool { self.tids.get(tid as usize).map_or(false, |t| t.is_some()) }

    /// Start (or restart) the session of `tid`; a running one is flushed
    /// into `out` first.
    pub fn start(&mut self, tid: u8, ssn: u16, buf_size: u16, out: &mut Vec<T>) {
        self.stop(tid, out);
        if let Some(s) = self.tids.get_mut(tid as usize) {
            *s = Some(Reorder::new(ssn, buf_size));
            self.sessions += 1;
        }
    }

    /// End the session of `tid`, its waiting frames into `out`.
    pub fn stop(&mut self, tid: u8, out: &mut Vec<T>) {
        if let Some(Some(mut r)) = self.tids.get_mut(tid as usize).map(|s| s.take()) {
            r.flush(out);
            self.ended.add(&r.stats);
        }
    }

    pub fn stop_all(&mut self, out: &mut Vec<T>) { for t in 0..8 { self.stop(t, out); } }

    /// Timeout releases of every session.
    pub fn tick(&mut self, now: u64, out: &mut Vec<T>) {
        for r in self.tids.iter_mut().flatten() { if r.stored() > 0 { r.release(now, out); } }
    }

    pub fn stored(&self) -> usize { self.tids.iter().flatten().map(|r| r.stored()).sum() }

    /// Counters of ended and running sessions.
    pub fn stats(&self) -> ReorderStats {
        let mut s = self.ended;
        for r in self.tids.iter().flatten() { s.add(&r.stats); }
        s
    }
}
