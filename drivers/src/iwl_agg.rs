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
//! * Block Ack Requests (lane bssidbar, L12 #63). Linux trusts the firmware
//!   here: iwl_mvm_tx_reclaim marks every frame a BA notification reclaims
//!   as acked, and only single-frame failures (AMPDU_NO_BACK) get a BAR,
//!   SSN = failed SN + 1, from mac80211; a BAR that is not acked is kept
//!   pending and re-sent after the next successful transmission
//!   (ieee80211_set_bar_pending / ieee80211_check_pending_bar). A router
//!   (84:78:48:d8:7e:79) held its reorder window at a hole after a burst of
//!   partially acked A-MPDUs and dropped everything above it until it
//!   deauthenticated us 5 min later, with no single failure and no BAR. So we
//!   also send a BAR after a partial BA, with SSN = the SN of the BA's
//!   scd_ssn: the first frame the firmware has not finished, so the BAR
//!   never skips a frame still being retried, only closes holes below it.
//!   BARs are rate limited (BAR_MIN_INTERVAL_MS), a failed one is retried,
//!   and after BAR_MAX_FAILS unacked BARs in a row the session is torn down
//!   (DELBA, as 802.11-2016 10.24.7 allows) and renegotiated later.
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
/// At most one BAR per this interval (a lossy burst gives many partial BAs).
pub const BAR_MIN_INTERVAL_MS: u64 = 50;
/// A BAR whose TX status never came counts as failed after this long.
pub const BAR_STATUS_TIMEOUT_MS: u64 = 1000;
/// Unacknowledged BARs in a row after which the session is torn down.
pub const BAR_MAX_FAILS: u32 = 3;

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
    /// BA notifications that acked fewer frames than were sent.
    pub ba_partial: u64,
    /// BARs the AP did not acknowledge (or whose status never came).
    pub bars_failed: u64,
    /// Sessions we ended because BARs kept failing.
    pub bar_teardowns: u64,
    /// Frames on the aggregation TID that failed alone (a BAR follows).
    pub single_fail: u64,
    /// Frames whose sequence number did not match the ring index (the
    /// scheduler requirement; should stay 0).
    pub misaligned: u64,
    /// BA notifications with no A-MPDU TX response before them: the AP's
    /// answer to our BAR (or a stray BA). They reclaim, but are neither
    /// partial nor rate-control samples.
    pub ba_unsolicited: u64,
    /// BAs whose bitmap shows a frame the firmware finished (below
    /// scd_ssn) that the AP never received: a hole only a BAR closes.
    pub ba_holes: u64,
}

/// One BA notification as kept for /proc/net/nic (newest last).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaRec {
    /// The BA's starting sequence number (the AP's window start).
    pub ba_ssn: u16,
    pub bitmap: u64,
    /// The SN of scd_ssn (the first frame the firmware has not finished).
    pub scd_sn: u16,
    pub txed: u8,
    pub done: u8,
    /// An A-MPDU TX response came before it.
    pub solicited: bool,
}

/// The number of BA records kept.
pub const BA_RECS: usize = 6;

/// A hole the AP waits for: the BA's window starts at `ba_ssn` with
/// `bitmap` (bit i = SN ba_ssn + i received), and the firmware has
/// finished every frame before `scd_sn`. A finished frame the bitmap does
/// not have (or a window start more than 64 behind) never comes again.
pub fn ba_hole(ba_ssn: u16, bitmap: u64, scd_sn: u16) -> bool {
    let d = scd_sn.wrapping_sub(ba_ssn) & 0xfff;
    if d == 0 || d >= 0x800 { return false; }
    if d >= 64 { return true; }
    let need = (1u64 << d) - 1;
    bitmap & need != need
}

/// The TX aggregation session of TID 0 with the AP.
#[derive(Clone, Copy, Debug)]
pub struct TxAgg {
    pub state: AggState,
    pub enabled: bool,
    pub tries: u32,
    pub next_try_ms: u64,
    next_dialog: u8,
    /// SSN a BAR should carry (a hole to close), not sent yet.
    bar_want: Option<u16>,
    /// The BAR on air (SSN, sent at), waiting for its TX status.
    bar_inflight: Option<(u16, u64)>,
    last_bar_ms: Option<u64>,
    /// An A-MPDU TX response awaits its BA notification.
    agg_pending: bool,
    pub recent: [BaRec; BA_RECS],
    bar_fails: u32,
    pub st: AggStats,
}

/// The sequence number of ring index `idx` on the aggregation queue, from
/// the next sequence number to assign (the numbering keeps SN = index mod
/// 256, see tx_data).
pub fn sn_of_index(next_sn: u16, idx: u16) -> u16 {
    next_sn.wrapping_sub(next_sn.wrapping_sub(idx) & 0xff) & 0xfff
}

fn sn_before(a: u16, b: u16) -> bool { (a.wrapping_sub(b) & 0xfff) > 0x800 }

impl TxAgg {
    pub fn new(enabled: bool) -> TxAgg {
        TxAgg { state: AggState::Off, enabled, tries: 0, next_try_ms: 0, next_dialog: 1, bar_want: None, bar_inflight: None,
                last_bar_ms: None, bar_fails: 0, agg_pending: false,
                recent: [BaRec::default(); BA_RECS], st: AggStats::default() }
    }

    /// A new association: no session, tries reset (counters kept).
    pub fn reset(&mut self) {
        self.state = AggState::Off;
        self.tries = 0;
        self.next_try_ms = 0;
        self.clear_bar();
    }

    fn clear_bar(&mut self) {
        self.bar_want = None;
        self.bar_inflight = None;
        self.last_bar_ms = None;
        self.bar_fails = 0;
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
        self.agg_pending = true;
    }

    /// A BA notification; `next_sn` is the next sequence number to assign.
    /// True when it answers an A-MPDU (an agg TX response came first): only
    /// then are its sent / acked counts a rate-control sample. The firmware
    /// also reports the AP's answer to our BAR; counting that one as a
    /// partial BA asked for the next BAR, a BAR loop at the BAR rate limit.
    /// A BAR is wanted only for a real hole ([`ba_hole`]): the firmware
    /// retries unacked frames itself while they stay at or above scd_ssn.
    pub fn on_ba(&mut self, n: &BaNotif, next_sn: u16) -> bool {
        self.st.ba_notifs += 1;
        let solicited = core::mem::replace(&mut self.agg_pending, false);
        let scd_sn = sn_of_index(next_sn, n.scd_ssn);
        let ba_ssn = n.seq_ctl >> 4;
        self.recent.copy_within(1.., 0);
        self.recent[BA_RECS - 1] = BaRec { ba_ssn, bitmap: n.bitmap, scd_sn, txed: n.txed, done: n.txed_2_done, solicited };
        if !solicited {
            self.st.ba_unsolicited += 1;
            return false;
        }
        self.st.ba_txed += n.txed as u64;
        self.st.ba_acked += n.txed_2_done as u64;
        if n.txed_2_done < n.txed { self.st.ba_partial += 1; }
        let hole = ba_hole(ba_ssn, n.bitmap, scd_sn);
        if hole { self.st.ba_holes += 1; }
        if self.on() && (hole || self.bar_want.is_some()) {
            self.want_bar(scd_sn);
        }
        true
    }

    /// A frame of the session failed alone (sequence number `sn`): the AP's
    /// window must move past it (mac80211, AMPDU_NO_BACK: SSN = SN + 1).
    pub fn on_single_fail(&mut self, sn: u16) {
        self.st.single_fail += 1;
        if self.on() { self.want_bar((sn + 1) & 0xfff); }
    }

    fn want_bar(&mut self, ssn: u16) {
        self.bar_want = Some(match self.bar_want { Some(w) if sn_before(ssn, w) => w, _ => ssn });
    }

    /// The BAR to send now, if one is due (and the previous one is done).
    pub fn next_bar(&mut self, now: u64) -> Option<u16> {
        if !self.on() { return None; }
        if let Some((ssn, at)) = self.bar_inflight {
            if now.saturating_sub(at) < BAR_STATUS_TIMEOUT_MS { return None; }
            // No TX status: count it as a failure (the teardown follows from
            // on_bar_status at the next one).
            self.bar_inflight = None;
            self.st.bars_failed += 1;
            self.bar_fails += 1;
            self.want_bar(ssn);
        }
        let ssn = self.bar_want?;
        if self.last_bar_ms.map_or(false, |t| now.saturating_sub(t) < BAR_MIN_INTERVAL_MS) { return None; }
        self.bar_want = None;
        self.bar_inflight = Some((ssn, now));
        self.last_bar_ms = Some(now);
        self.st.bars_sent += 1;
        Some(ssn)
    }

    /// The BAR could not be queued: keep it wanted.
    pub fn bar_not_sent(&mut self) {
        if let Some((ssn, _)) = self.bar_inflight.take() { self.st.bars_sent -= 1; self.want_bar(ssn); }
    }

    /// The BAR's TX status. True when the session must be torn down now
    /// (BAR_MAX_FAILS in a row): the state is already Off and a new ADDBA
    /// may follow after ADDBA_RETRY_MS; the caller sends the DELBA.
    pub fn on_bar_status(&mut self, acked: bool, now: u64) -> bool {
        let (ssn, _) = match self.bar_inflight.take() { Some(b) => b, None => return false };
        if acked { self.bar_fails = 0; return false; }
        self.st.bars_failed += 1;
        self.bar_fails += 1;
        if self.bar_fails >= BAR_MAX_FAILS && self.on() {
            self.st.bar_teardowns += 1;
            self.state = AggState::Off;
            self.next_try_ms = now + ADDBA_RETRY_MS;
            self.tries = 0;
            self.clear_bar();
            return true;
        }
        // Retried after the next transmission (ieee80211_set_bar_pending).
        self.want_bar(ssn);
        false
    }

    /// Whether a BAR's TX status is awaited.
    pub fn bar_inflight(&self) -> bool { self.bar_inflight.is_some() }

    pub fn line(&self) -> String {
        let s = &self.st;
        let state = match self.state {
            AggState::Off if !self.enabled => String::from("off (iwlwifi.txagg=0)"),
            AggState::Off => String::from("off"),
            AggState::Requested { dialog, ssn, .. } => format!("requested (dialog {} SSN {})", dialog, ssn),
            AggState::On { buf } => format!("on win{}", buf),
        };
        let avg10 = if s.agg_rsps > 0 { s.agg_frames * 10 / s.agg_rsps } else { 0 };
        format!("{}; addba {} accepted {} refused {} (last status {}) timeouts {} delba {}; ampdu {} frames {} avg {}.{} max {}; ba {} txed {} acked {}; single_fail {} partial {} bars {} bar_fail {} bar_teardown {} misaligned {} unsolicited {} holes {}; last {}",
                state, s.addba_sent, s.accepted, s.refused, s.last_refusal, s.timeouts, s.delba_rx, s.agg_rsps, s.agg_frames,
                avg10 / 10, avg10 % 10, s.agg_max, s.ba_notifs, s.ba_txed, s.ba_acked, s.single_fail, s.ba_partial, s.bars_sent, s.bars_failed,
                s.bar_teardowns, s.misaligned, s.ba_unsolicited, s.ba_holes,
                self.recent.iter().filter(|r| r.txed > 0 || r.bitmap != 0).map(|r| format!("{}:{:x}@{}/{}/{}{}", r.ba_ssn, r.bitmap, r.scd_sn,
                    r.txed, r.done, if r.solicited { "" } else { "u" })).collect::<alloc::vec::Vec<_>>().join(" "))
    }
}

/// Outcome of [`sim_bar_hole`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoleSim {
    /// Time (ms) from the hole to the first in-order delivery past it.
    pub resume_ms: Option<u64>,
    pub delivered: u64,
    /// Frames the recipient dropped (beyond its stuck window).
    pub dropped: u64,
    pub bars: u64,
    pub teardown: bool,
}

/// Uplink Block Ack guard (lane bssidbar, L12 #63; wifisim `ba-hole-bar`
/// and drivers/iwlwifi-tests). Our TxAgg is the originator; the "firmware"
/// sends A-MPDUs of up to 16 frames every 2 ms within a 64-frame scheduler
/// window and, in the burst at 100 ms, gives up on `lost` subframes
/// silently (never block acked, reclaimed by the next BA's scd_ssn: the
/// partial BAs seen on the laptop). The recipient behaves like the router:
/// it holds its reorder window at a hole until a BAR moves it, or for
/// `hold_ms`, and drops (after the MAC ACK) every frame beyond the window.
/// The sender is ACK clocked, so a stuck window stalls it (no SN wrap).
/// `bar_acked` false: the AP never acks a BAR. Runs `run_ms` of sim time.
pub fn sim_bar_hole(lost: u16, hold_ms: u64, bar_acked: bool, run_ms: u64) -> HoleSim {
    let mut a = TxAgg::new(true);
    a.poll_request(0, true, true, true, 0);
    a.on_addba_resp(1, 0, AGG_TID, AGG_WINDOW, 0);
    let mut r = HoleSim::default();
    let (mut next_sn, mut fw_head) = (0u16, 0u16); // fw_head: first frame not finished
    let (mut rx_head, mut rx_got) = (0u16, [false; 4096]);
    let mut hole_at: Option<(u64, u16)> = None; // (when, first lost SN)
    let mut held_since: Option<u64> = None;
    let (mut now, mut sent) = (0u64, 0u64);
    while now < run_ms {
        // Keep the 64-frame scheduler window full, as a bulk TCP flow with
        // 256 packets in flight does (it stalls when nothing is delivered).
        while next_sn.wrapping_sub(fw_head) & 0xfff < AGG_WINDOW && sent < r.delivered + 256 {
            next_sn = (next_sn + 1) & 0xfff;
            sent += 1;
        }
        if !a.on() { r.teardown = true; break; }
        // One A-MPDU of up to 16 frames from the window start.
        let n = (next_sn.wrapping_sub(fw_head) & 0xfff).min(16);
        let lose = now == 100;
        let mut done = 0u8;
        for i in 0..n {
            let sn = (fw_head + i) & 0xfff;
            if lose && i >= 4 && i < 4 + lost {
                if hole_at.is_none() { hole_at = Some((now, sn)); }
                continue;
            }
            done += 1;
            // Recipient: inside the window -> buffered/delivered, else dropped.
            if sn.wrapping_sub(rx_head) & 0xfff >= AGG_WINDOW { r.dropped += 1; continue; }
            rx_got[sn as usize] = true;
        }
        fw_head = (fw_head + n) & 0xfff; // all finished (the lost ones given up)
        // The AP's compressed BA: its window start and what it holds.
        let bitmap = (0..64u16).filter(|&k| rx_got[((rx_head + k) & 0xfff) as usize]).fold(0u64, |m, k| m | 1 << k);
        let ba = BaNotif { sta_addr: [0; 6], sta_id: 0, tid: AGG_TID, seq_ctl: rx_head << 4, bitmap, scd_flow: 0,
                           scd_ssn: fw_head & 0xff, txed: n as u8, txed_2_done: done, reduced_txp: 0 };
        if n > 0 { a.on_agg_rsp(n as u8); a.on_ba(&ba, next_sn); }
        if let Some(ssn) = a.next_bar(now) {
            r.bars += 1;
            if bar_acked {
                if ssn.wrapping_sub(rx_head) & 0xfff < 0x800 {
                    while rx_head != ssn { rx_got[rx_head as usize] = false; rx_head = (rx_head + 1) & 0xfff; }
                }
            }
            if a.on_bar_status(bar_acked, now) { r.teardown = true; break; }
        }
        // In-order release; a hole is held for hold_ms.
        loop {
            if rx_got[rx_head as usize] {
                rx_got[rx_head as usize] = false;
                r.delivered += 1;
                if let Some((t, hsn)) = hole_at {
                    if r.resume_ms.is_none() && rx_head.wrapping_sub(hsn) & 0xfff < 0x800 { r.resume_ms = Some(now - t); }
                }
                rx_head = (rx_head + 1) & 0xfff;
                held_since = None;
                continue;
            }
            let waiting = (1..AGG_WINDOW).any(|k| rx_got[((rx_head + k) & 0xfff) as usize]);
            if !waiting { break; }
            let t = *held_since.get_or_insert(now);
            if now - t < hold_ms { break; }
            rx_head = (rx_head + 1) & 0xfff;
            held_since = None;
        }
        now += 2;
    }
    r
}
