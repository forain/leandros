//! TX aggregation (A-MPDU from us to the AP) for an iwlwifi station, the
//! way Linux iwlmvm does it on the 8000 family (gen1 PCIe, DQA, driver rate
//! control): hardware independent and host tested (drivers/iwlwifi-tests).
//!
//! * The BSS data queue is configured as an aggregation queue (SCD_QUEUE_CFG
//!   `aggregate`, TID 0) at association, as iwl_mvm_sta_alloc_queue does
//!   for every DQA data queue. The scheduler then needs each frame's 802.11
//!   sequence number to equal its ring index (mod 256, the check in
//!   iwl_trans_pcie_tx): the driver numbers QoS data itself and starts the
//!   numbering at the ring's write index.
//! * Once the link is up and the queue is empty (iwl_mvm_sta_tx_agg_start's
//!   "normalized_ssn == next_reclaimed"), we send an ADDBA request for TID 0
//!   (immediate policy, window 64, SSN = the next sequence number).
//! * On a successful ADDBA response, ADD_STA clears the TID's bit in
//!   `tid_disable_tx` (iwl_mvm_sta_tx_agg) and the LQ frame limit becomes
//!   min(63, the AP's window) (iwl_mvm_sta_tx_agg_oper). The firmware then
//!   builds A-MPDUs on its own.
//! * TX responses with frame_count > 1 describe an A-MPDU: no reclaim, only
//!   the rate (iwl_mvm_rx_tx_cmd_agg). The BA notification (0xc5) reclaims
//!   the ring up to its scd_ssn and carries sent / acked counts for rate
//!   control (iwl_mvm_rx_ba_notif). The firmware sends a frame alone at
//!   least once before giving up on it, so failures arrive as single TX
//!   responses; for those mac80211 sends a BAR so the AP's reorder buffer
//!   does not wait for the frame (IEEE80211_TX_STAT_AMPDU_NO_BACK).
//! * A DELBA from the AP (recipient) ends the session; tid_disable_tx is
//!   set again. A missing ADDBA response (1 s) is retried twice, 10 s apart.

use alloc::format;
use alloc::string::String;

use crate::iwl_cmd::{le16, le64};

/// The TID we aggregate (all our data goes out as TID 0, best effort).
pub const AGG_TID: u8 = 0;
/// Our ADDBA window (the 8000 family's scheduler window).
pub const AGG_WINDOW: u16 = 64;
/// LQ agg_frame_cnt_limit ceiling on gen1 (LINK_QUAL_AGG_FRAME_LIMIT_MAX).
pub const LQ_AGG_FRAME_LIMIT_MAX: u8 = 63;
pub const ADDBA_TIMEOUT_MS: u64 = 1000;
pub const ADDBA_RETRY_MS: u64 = 10_000;
pub const ADDBA_MAX_TRIES: u32 = 3;

/// BA_NOTIF (REPLY_COMPRESSED_BA, struct iwl_mvm_ba_notif, 28 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaNotif {
    pub sta_addr: [u8; 6],
    pub sta_id: u8,
    pub tid: u8,
    pub seq_ctl: u16,
    pub bitmap: u64,
    /// The TX queue ("flow").
    pub scd_flow: u16,
    /// Start of the Block Ack window: the ring index of the first frame
    /// not yet acknowledged (reclaim everything before it).
    pub scd_ssn: u16,
    /// Frames sent in the A-MPDU and acknowledged by the BA.
    pub txed: u8,
    pub txed_2_done: u8,
    pub reduced_txp: u8,
}

pub fn parse_ba_notif(p: &[u8]) -> Option<BaNotif> {
    if p.len() < 28 { return None; }
    let mut a = [0u8; 6];
    a.copy_from_slice(&p[0..6]);
    Some(BaNotif { sta_addr: a, sta_id: p[8], tid: p[9], seq_ctl: le16(p, 10)?, bitmap: le64(p, 12)?, scd_flow: le16(p, 20)?,
                   scd_ssn: le16(p, 22)?, txed: p[24], txed_2_done: p[25], reduced_txp: p[26] })
}

/// One subframe of an aggregated TX response: (AGG_TX_STATE status, the
/// TX command sequence it belongs to).
pub fn agg_statuses(p: &[u8]) -> alloc::vec::Vec<(u16, u16)> {
    let n = p.first().copied().unwrap_or(0) as usize;
    (0..n).filter_map(|i| Some((le16(p, 36 + 4 * i)?, le16(p, 38 + 4 * i)?))).collect()
}

/// AGG_TX_STATE_STATUS_MSK: 0 = transmitted.
pub const AGG_TX_STATE_STATUS_MSK: u16 = 0x0fff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggState {
    /// No session (not tried yet, or given up / torn down).
    Off,
    /// ADDBA request sent at `since_ms` with this dialog token and SSN.
    Requested { dialog: u8, ssn: u16, since_ms: u64 },
    /// Session up with the AP's window.
    On { buf: u16 },
}

/// Counters of the TX Block Ack session (/proc/net/nic `tx_ba:`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AggStats {
    pub addba_sent: u64,
    pub accepted: u64,
    pub refused: u64,
    pub last_refusal: u16,
    pub timeouts: u64,
    pub delba_rx: u64,
    /// TX responses for A-MPDUs and the subframes they reported.
    pub agg_rsps: u64,
    pub agg_frames: u64,
    pub agg_max: u8,
    /// BA notifications, and their sent / acked totals.
    pub ba_notifs: u64,
    pub ba_txed: u64,
    pub ba_acked: u64,
    pub bars_sent: u64,
    /// Frames on the aggregation TID that failed alone (a BAR follows).
    pub single_fail: u64,
    /// Frames whose sequence number did not match the ring index (the
    /// scheduler requirement; should stay 0).
    pub misaligned: u64,
}

/// The TX aggregation session of TID 0 with the AP.
#[derive(Clone, Copy, Debug)]
pub struct TxAgg {
    pub state: AggState,
    pub enabled: bool,
    pub tries: u32,
    pub next_try_ms: u64,
    next_dialog: u8,
    pub st: AggStats,
}

impl TxAgg {
    pub fn new(enabled: bool) -> TxAgg {
        TxAgg { state: AggState::Off, enabled, tries: 0, next_try_ms: 0, next_dialog: 1, st: AggStats::default() }
    }

    /// A new association: no session, tries reset (counters kept).
    pub fn reset(&mut self) {
        self.state = AggState::Off;
        self.tries = 0;
        self.next_try_ms = 0;
    }

    /// A new association with TX aggregation `enabled` or not.
    pub fn restart(&mut self, enabled: bool) {
        self.reset();
        self.enabled = enabled;
    }

    pub fn on(&self) -> bool { matches!(self.state, AggState::On { .. }) }

    /// Whether to send an ADDBA request now: HT link with the keys in
    /// place, the data queue empty, no session, tries left. Returns the
    /// (dialog token, SSN) to send and moves to Requested.
    pub fn poll_request(&mut self, now: u64, ht: bool, authorized: bool, queue_empty: bool, next_seq: u16) -> Option<(u8, u16)> {
        if let AggState::Requested { since_ms, .. } = self.state {
            if now.saturating_sub(since_ms) >= ADDBA_TIMEOUT_MS {
                self.st.timeouts += 1;
                self.state = AggState::Off;
                self.next_try_ms = now + ADDBA_RETRY_MS;
            }
            return None;
        }
        if !self.enabled || !ht || !authorized || !queue_empty || self.state != AggState::Off { return None; }
        if self.tries >= ADDBA_MAX_TRIES || now < self.next_try_ms { return None; }
        let dialog = self.next_dialog;
        self.next_dialog = self.next_dialog.wrapping_add(1).max(1);
        self.tries += 1;
        self.st.addba_sent += 1;
        self.state = AggState::Requested { dialog, ssn: next_seq & 0xfff, since_ms: now };
        Some((dialog, next_seq & 0xfff))
    }

    /// The AP's ADDBA response. Some(window) when the session should be
    /// switched on now (the caller then sends ADD_STA and the LQ).
    pub fn on_addba_resp(&mut self, dialog: u8, status: u16, tid: u8, buf: u16, now: u64) -> Option<u16> {
        let ok_dialog = matches!(self.state, AggState::Requested { dialog: d, .. } if d == dialog);
        if !ok_dialog || tid != AGG_TID { return None; }
        if status != 0 {
            self.st.refused += 1;
            self.st.last_refusal = status;
            self.state = AggState::Off;
            self.next_try_ms = now + ADDBA_RETRY_MS;
            return None;
        }
        let w = if buf == 0 { AGG_WINDOW } else { buf.min(AGG_WINDOW) };
        self.st.accepted += 1;
        self.state = AggState::On { buf: w };
        Some(w)
    }

    /// The firmware refused the session (ADD_STA failed): off for good on
    /// this association.
    pub fn fw_refused(&mut self) {
        self.state = AggState::Off;
        self.tries = ADDBA_MAX_TRIES;
    }

    /// A DELBA from the AP for our session. True when a session was up
    /// (the caller then sets tid_disable_tx again).
    pub fn on_delba(&mut self, tid: u8) -> bool {
        if tid != AGG_TID { return false; }
        self.st.delba_rx += 1;
        let was = self.on() || matches!(self.state, AggState::Requested { .. });
        self.state = AggState::Off;
        self.tries = ADDBA_MAX_TRIES;
        was
    }

    /// LQ agg_frame_cnt_limit for the current state.
    pub fn lq_frame_limit(&self) -> u8 {
        match self.state {
            AggState::On { buf } => (buf.min(LQ_AGG_FRAME_LIMIT_MAX as u16)) as u8,
            _ => LQ_AGG_FRAME_LIMIT_MAX,
        }
    }

    pub fn on_agg_rsp(&mut self, frames: u8) {
        self.st.agg_rsps += 1;
        self.st.agg_frames += frames as u64;
        self.st.agg_max = self.st.agg_max.max(frames);
    }

    pub fn on_ba(&mut self, n: &BaNotif) {
        self.st.ba_notifs += 1;
        self.st.ba_txed += n.txed as u64;
        self.st.ba_acked += n.txed_2_done as u64;
    }

    pub fn line(&self) -> String {
        let s = &self.st;
        let state = match self.state {
            AggState::Off if !self.enabled => String::from("off (iwlwifi.txagg=0)"),
            AggState::Off => String::from("off"),
            AggState::Requested { dialog, ssn, .. } => format!("requested (dialog {} SSN {})", dialog, ssn),
            AggState::On { buf } => format!("on win{}", buf),
        };
        let avg10 = if s.agg_rsps > 0 { s.agg_frames * 10 / s.agg_rsps } else { 0 };
        format!("{}; addba {} accepted {} refused {} (last status {}) timeouts {} delba {}; ampdu {} frames {} avg {}.{} max {}; ba {} txed {} acked {}; single_fail {} bars {} misaligned {}",
                state, s.addba_sent, s.accepted, s.refused, s.last_refusal, s.timeouts, s.delba_rx, s.agg_rsps, s.agg_frames,
                avg10 / 10, avg10 % 10, s.agg_max, s.ba_notifs, s.ba_txed, s.ba_acked, s.single_fail, s.bars_sent, s.misaligned)
    }
}
