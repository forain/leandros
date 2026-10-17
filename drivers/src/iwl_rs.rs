//! TX rate control for a legacy (non-HT) iwlwifi station, and the TX
//! statistics the driver keeps. Hardware independent and host tested
//! (drivers/iwlwifi-tests).
//!
//! Rate control is a reduced form of Linux iwlwifi `mvm/rs.c` for legacy
//! rates, one column per antenna:
//! * initial rate from the beacon RSSI (rs_get_initial_rate's legacy tables);
//! * per-rate success ratio from every TX response: each attempt of the
//!   retry chain (failure_frame + 1 of them, read off the table the firmware
//!   had) counts a failure, the last one the frame's verdict
//!   (rs_collect_tlc_data); kept as an EWMA plus the counts that make a
//!   window "valid" (8 successes or 3 failures);
//! * the move up / down / stay of rs_get_rate_action on expected throughput
//!   (success ratio x Linux's expected_tpt_legacy);
//! * the station table (LQ_CMD) starts at the chosen rate: two tries there,
//!   one at each lower rate down to the lowest basic rate, which fills the
//!   rest; it is resent at most every 100 ms;
//! * antenna A/B toggle (when both are valid and none is forced) after a
//!   run of frames that failed outright, or when even the lowest rate fails.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::iwl_assoc::{self as asc, TxResp, ANT_A, ANT_B, LEGACY_R500K, RATE_ID_MSK};

/// Linux rs.c expected_tpt_legacy, in the order of `LEGACY_R500K`.
pub const EXPECTED_TPT: [u32; 12] = [7, 13, 35, 40, 57, 58, 72, 98, 121, 154, 177, 186];
/// Success ratio (permille) at or below which the rate goes down at once.
pub const SR_FORCE_DECREASE: u32 = 150;
/// Success ratio (permille) at or above which the rate never goes down.
pub const SR_NO_DECREASE: u32 = 850;
pub const MIN_SUCCESS: u32 = 8;
pub const MIN_FAILURE: u32 = 3;
/// A rate's window older than this is forgotten (unknown again).
pub const STALE_MS: u64 = 3000;
/// The station table goes to the firmware at most this often.
pub const LQ_MIN_INTERVAL_MS: u64 = 100;
/// Frames in a row that failed outright before the antenna is toggled.
pub const FAIL_STREAK_TOGGLE: u32 = 8;
pub const TOGGLE_MIN_MS: u64 = 2000;

fn legacy_index(r500k: u8) -> Option<usize> { LEGACY_R500K.iter().position(|&r| r == r500k & 0x7f) }

fn expected_tpt(r500k: u8) -> u32 { legacy_index(r500k).map_or(0, |i| EXPECTED_TPT[i]) }

/// Mb/s of a rate as text (5.5 for 11).
pub fn mbps(r500k: u8) -> String {
    if r500k % 2 == 1 { format!("{}.5", r500k / 2) } else { format!("{}", r500k / 2) }
}

/// Linux rs_get_initial_rate's legacy tables: the fastest rate worth trying
/// first at a beacon signal of `rssi` dBm.
pub fn initial_r500k(rssi: i8, band_24: bool) -> u8 {
    let t24: [(i8, u8); 7] = [(-60, 108), (-64, 96), (-68, 72), (-80, 48), (-84, 36), (-85, 24), (i8::MIN, 2)];
    let t5: [(i8, u8); 8] = [(-60, 108), (-64, 96), (-72, 72), (-80, 48), (-84, 36), (-85, 24), (-87, 18), (i8::MIN, 12)];
    let t: &[(i8, u8)] = if band_24 { &t24 } else { &t5 };
    t.iter().find(|&&(th, _)| rssi > th || th == i8::MIN).map_or(12, |&(_, r)| r)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateWin {
    pub att: u32,
    pub succ: u32,
    /// Success ratio, permille (EWMA, 1/8 weight; the plain ratio for the
    /// first 8 attempts).
    pub sr: u32,
    pub last_ms: u64,
}

impl RateWin {
    pub fn record(&mut self, ok: bool, now: u64) {
        if self.att > 0 && now.saturating_sub(self.last_ms) > STALE_MS { *self = RateWin::default(); }
        self.att = self.att.saturating_add(1);
        if ok { self.succ = self.succ.saturating_add(1); }
        let x = if ok { 1000 } else { 0 };
        self.sr = if self.att <= 8 { self.succ * 1000 / self.att } else { (self.sr * 7 + x) / 8 };
        self.last_ms = now;
    }
    pub fn fresh(&self, now: u64) -> bool { self.att > 0 && now.saturating_sub(self.last_ms) <= STALE_MS }
    /// Enough attempts for a judgment (Linux: 8 successes or 3 failures).
    pub fn valid(&self, now: u64) -> bool { self.fresh(now) && (self.succ >= MIN_SUCCESS || self.att - self.succ >= MIN_FAILURE) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action { Stay, Up, Down }

/// rs_get_rate_action. `low` / `high`: None when there is no such rate,
/// Some(None) when it exists but its throughput is unknown. Throughputs are
/// success permille x expected_tpt; `low_expected` is the lower rate's
/// expected_tpt (its best case).
pub fn rate_action(sr: u32, cur_tpt: u32, low: Option<Option<u32>>, high: Option<Option<u32>>, low_expected: u32) -> Action {
    if sr <= SR_FORCE_DECREASE || cur_tpt == 0 { return Action::Down; }
    let (low_tpt, high_tpt) = (low.flatten(), high.flatten());
    let has_high = high.is_some();
    let has_low = low.is_some();
    let mut action = Action::Stay;
    if low_tpt.is_none() && high_tpt.is_none() && has_high { return Action::Up; }
    if high_tpt.is_none() && has_high && low_tpt.map_or(false, |l| l < cur_tpt) { return Action::Up; }
    if high_tpt.map_or(false, |h| h > cur_tpt) { return Action::Up; }
    if low_tpt.map_or(false, |l| l < cur_tpt) && high_tpt.map_or(false, |h| h < cur_tpt) { return Action::Stay; }
    if low_tpt.map_or(false, |l| l > cur_tpt) || (low_tpt.is_none() && has_low) { action = Action::Down; }
    if action == Action::Down && has_low {
        if sr >= SR_NO_DECREASE || cur_tpt > 1000 * low_expected { action = Action::Stay; }
    }
    action
}

/// What rate control starts from.
#[derive(Clone, Debug)]
pub struct RateCfg {
    /// The AP's rates (Supported + Extended, 500 kb/s units, 0x80 = basic).
    pub ap_rates: Vec<u8>,
    /// Fastest rate allowed (0 = no cap), 500 kb/s units.
    pub max_r500k: u8,
    /// Antennas the firmware / NVM allow for TX.
    pub valid_ant: u8,
    /// A forced antenna (iwlwifi.txant=a|b): no toggling.
    pub fixed_ant: Option<u8>,
    pub band_24: bool,
    /// Signal of the AP's beacons, if known.
    pub rssi: Option<i8>,
}

/// Legacy rate control for one station.
#[derive(Clone, Debug)]
pub struct RateCtl {
    /// Usable rates, slowest first, 500 kb/s units.
    pub rates: Vec<u8>,
    /// Index of the lowest basic rate (the floor of the retry chain).
    pub floor: usize,
    pub win: Vec<RateWin>,
    /// Index of the rate the table starts at.
    pub cur: usize,
    pub ant: u8,
    valid_ant: u8,
    ant_fixed: bool,
    /// The table the firmware has (None before the first LQ_CMD).
    pub sent: Option<[u32; 16]>,
    sent_ms: u64,
    /// The wanted table differs from the one sent.
    pub pending: bool,
    pub fail_streak: u32,
    last_toggle_ms: u64,
    pub ups: u64,
    pub downs: u64,
    pub toggles: u64,
    pub lq_sent: u64,
    /// TX responses whose initial rate is not the table's (sent before the
    /// last LQ_CMD, or at a fixed rate): not counted.
    pub mismatched: u64,
}

impl RateCtl {
    pub fn new(cfg: &RateCfg) -> RateCtl {
        let mut rates: Vec<u8> = cfg.ap_rates.iter().map(|r| r & 0x7f)
            .filter(|&r| legacy_index(r).is_some() && (cfg.max_r500k == 0 || r <= cfg.max_r500k)).collect();
        rates.sort_unstable_by_key(|&r| legacy_index(r));
        rates.dedup();
        if rates.is_empty() { rates.push(if cfg.band_24 { 2 } else { 12 }); }
        let floor = cfg.ap_rates.iter().filter(|r| *r & 0x80 != 0)
            .filter_map(|r| rates.iter().position(|&x| x == r & 0x7f)).min().unwrap_or(0);
        // Unknown signal: 24 Mb/s, the middle of the table.
        let want = cfg.rssi.map_or(48, |s| initial_r500k(s, cfg.band_24));
        let cur = rates.iter().rposition(|&r| legacy_index(r) <= legacy_index(want)).unwrap_or(0);
        let both = cfg.valid_ant & (ANT_A | ANT_B) == ANT_A | ANT_B;
        let (ant, ant_fixed) = match cfg.fixed_ant {
            Some(a) if cfg.valid_ant & a != 0 => (a, true),
            _ => (if cfg.valid_ant & ANT_A != 0 || cfg.valid_ant == 0 { ANT_A } else { ANT_B }, !both),
        };
        let n = rates.len();
        RateCtl { rates, floor, win: alloc::vec![RateWin::default(); n], cur, ant, valid_ant: cfg.valid_ant, ant_fixed,
                  sent: None, sent_ms: 0, pending: true, fail_streak: 0, last_toggle_ms: 0,
                  ups: 0, downs: 0, toggles: 0, lq_sent: 0, mismatched: 0 }
    }

    pub fn cur_r500k(&self) -> u8 { self.rates[self.cur] }

    /// The retry chain for the current rate and antenna: two tries at the
    /// current rate, one at each lower rate down to the lowest basic rate,
    /// which fills the rest.
    pub fn table(&self) -> [u32; 16] {
        let r = |i: usize| asc::legacy_rate_ant(self.rates[i], self.ant).unwrap_or(0);
        let floor = self.floor.min(self.cur);
        let mut t = [r(floor); 16];
        let mut n = 0;
        t[n] = r(self.cur); n += 1;
        t[n] = r(self.cur); n += 1;
        let mut i = self.cur;
        while i > floor && n < 16 { i -= 1; t[n] = r(i); n += 1; }
        t
    }

    pub fn lq(&self, sta_id: u8) -> Vec<u8> { asc::lq_cmd_table(sta_id, &self.table(), self.ant) }

    /// The LQ_CMD to send now, if the wanted table changed and the last one
    /// went out at least LQ_MIN_INTERVAL_MS ago (it then counts as sent).
    pub fn take_lq(&mut self, sta_id: u8, now: u64) -> Option<Vec<u8>> {
        if !self.due(now) { return None; }
        let lq = self.lq(sta_id);
        self.mark_sent(now);
        Some(lq)
    }

    /// A changed table may go to the firmware now.
    pub fn due(&self, now: u64) -> bool {
        self.pending && (self.sent.is_none() || now.saturating_sub(self.sent_ms) >= LQ_MIN_INTERVAL_MS)
    }

    pub fn mark_sent(&mut self, now: u64) {
        self.sent = Some(self.table());
        self.sent_ms = now;
        self.pending = false;
        self.lq_sent += 1;
    }

    /// The rate_n_flags of a frame's last attempt (its "final rate"), from
    /// the table the firmware had; the initial rate when it is not that table.
    pub fn final_rate(&self, r: &TxResp) -> u32 {
        match self.sent {
            Some(t) if t[0] & RATE_ID_MSK == r.initial_rate & RATE_ID_MSK => t[(r.failure_frame as usize).min(15)],
            _ => r.initial_rate,
        }
    }

    fn index_of(&self, rate_n_flags: u32) -> Option<usize> {
        let r = asc::rate_r500k(rate_n_flags)?;
        self.rates.iter().position(|&x| x == r)
    }

    fn can_toggle(&self, now: u64) -> bool {
        !self.ant_fixed && self.valid_ant & (ANT_A | ANT_B) == ANT_A | ANT_B
            && (self.toggles == 0 || now.saturating_sub(self.last_toggle_ms) >= TOGGLE_MIN_MS)
    }

    fn toggle(&mut self, now: u64) {
        self.ant = if self.ant == ANT_A { ANT_B } else { ANT_A };
        for w in self.win.iter_mut() { *w = RateWin::default(); }
        self.fail_streak = 0;
        self.toggles += 1;
        self.last_toggle_ms = now;
        self.pending = true;
    }

    /// One TX response of a data frame sent through the station table.
    /// Returns what was decided.
    pub fn on_tx(&mut self, r: &TxResp, now: u64) -> Action {
        let t = match self.sent { Some(t) => t, None => return Action::Stay };
        if t[0] & RATE_ID_MSK != r.initial_rate & RATE_ID_MSK { self.mismatched += 1; return Action::Stay; }
        let ok = r.ok();
        if !ok && !r.air_fail() { return Action::Stay; }
        let tries = (r.failure_frame as usize).min(15);
        for (i, &rate) in t.iter().enumerate().take(tries + 1) {
            if let Some(idx) = self.index_of(rate) { self.win[idx].record(i == tries && ok, now); }
        }
        if ok { self.fail_streak = 0; } else { self.fail_streak += 1; }
        self.decide(now)
    }

    fn tpt(&self, i: usize, now: u64) -> Option<u32> {
        let w = &self.win[i];
        if w.valid(now) { Some(w.sr * expected_tpt(self.rates[i])) } else { None }
    }

    fn decide(&mut self, now: u64) -> Action {
        // One step per table that reached the firmware.
        if self.pending { return Action::Stay; }
        if self.fail_streak >= FAIL_STREAK_TOGGLE && self.can_toggle(now) {
            self.toggle(now);
            return Action::Stay;
        }
        let c = self.cur;
        if !self.win[c].valid(now) { return Action::Stay; }
        let sr = self.win[c].sr;
        let cur_tpt = sr * expected_tpt(self.rates[c]);
        let low = if c > 0 { Some(self.tpt(c - 1, now)) } else { None };
        let high = if c + 1 < self.rates.len() { Some(self.tpt(c + 1, now)) } else { None };
        let low_expected = if c > 0 { expected_tpt(self.rates[c - 1]) } else { 0 };
        let a = rate_action(sr, cur_tpt, low, high, low_expected);
        match a {
            Action::Up if high.is_some() => { self.cur += 1; self.ups += 1; self.pending = true; }
            Action::Down if c > 0 => { self.cur -= 1; self.downs += 1; self.pending = true; }
            Action::Down => {
                // Already at the slowest rate and it fails: the other antenna.
                if sr <= SR_FORCE_DECREASE && self.can_toggle(now) { self.toggle(now); }
                return Action::Stay;
            }
            _ => return Action::Stay,
        }
        a
    }

    /// One line: current rate, antenna, per-rate success.
    pub fn summary(&self, now: u64) -> String {
        let mut s = format!("rc {}M ant {} up {} down {} toggles {} lq {} mismatched {}; sr",
                            mbps(self.cur_r500k()), if self.ant == ANT_B { "B" } else { "A" },
                            self.ups, self.downs, self.toggles, self.lq_sent, self.mismatched);
        for (i, w) in self.win.iter().enumerate() {
            if w.fresh(now) { let _ = write!(s, " {}M:{}%/{}", mbps(self.rates[i]), w.sr / 10, w.att); }
        }
        s
    }
}

// ── TX statistics ───────────────────────────────────────────────────────────

/// TX status buckets (low byte of the TX response status).
pub const STATUS_NAMES: [&str; 9] = ["01", "02", "4x", "82", "83", "88", "8a", "90", "other"];
pub const STATUS_MEANING: [&str; 9] = [
    "success", "direct done", "postponed (4x: delay / BT prio / quiet / TX power calc)",
    "short limit (no CTS to RTS)", "long limit (no ACK after all retries)", "dest in power save",
    "BT retry (coex killed it)", "dropped by firmware", "other",
];

pub fn status_bucket(status: u16) -> usize {
    match (status & 0xff) as u8 {
        0x01 => 0, 0x02 => 1, 0x40..=0x4f => 2, 0x82 => 3, 0x83 => 4, 0x88 => 5, 0x8a => 6, 0x90 => 7, _ => 8,
    }
}

/// Final-rate buckets: the legacy rates (LEGACY_R500K order), then other.
pub fn rate_bucket(rate_n_flags: u32) -> usize {
    asc::rate_r500k(rate_n_flags).and_then(legacy_index).unwrap_or(12)
}

/// Why a frame never reached the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqFail {
    /// The transport ring is full (or the frame too large).
    Busy,
    /// The queue is not set up (link going up / down).
    NotReady,
    /// Over the driver's queue-depth cap (backpressure missed).
    Capped,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxStats {
    /// Data frames queued to the firmware.
    pub enq: u64,
    /// Data frames the firmware reported delivered (ACKed).
    pub ok: u64,
    /// Data frames the firmware reported failed (TX_RSP status not success).
    pub fail_fw: u64,
    pub busy: u64,
    pub notready: u64,
    pub capped: u64,
    /// TX response status histogram (data queue), STATUS_NAMES order.
    pub status: [u64; 9],
    /// failure_frame (retries) of delivered / failed data frames, 15 = 15+.
    pub tries_ok: [u64; 16],
    pub tries_fail: [u64; 16],
    /// Final rate of delivered / failed data frames (rate_bucket).
    pub rate_ok: [u64; 13],
    pub rate_fail: [u64; 13],
    /// Attempts killed by Bluetooth coexistence, and frames with any.
    pub bt_kill: u64,
    pub bt_kill_frames: u64,
    /// RTS attempts without CTS.
    pub rts_fail: u64,
    /// Frames sent at reduced TX power.
    pub tpc_reduced: u64,
    pub mgmt_ok: u64,
    pub mgmt_fail: u64,
}

fn sub_arr<const N: usize>(a: &[u64; N], b: &[u64; N]) -> [u64; N] {
    let mut o = [0u64; N];
    for i in 0..N { o[i] = a[i].saturating_sub(b[i]); }
    o
}

impl TxStats {
    pub fn on_enqueue(&mut self, r: Result<(), EnqFail>) {
        match r {
            Ok(()) => self.enq += 1,
            Err(EnqFail::Busy) => self.busy += 1,
            Err(EnqFail::NotReady) => self.notready += 1,
            Err(EnqFail::Capped) => self.capped += 1,
        }
    }

    /// A TX response; `data` for the data queue, `final_rate` the rate of
    /// the last attempt.
    pub fn on_resp(&mut self, r: &TxResp, final_rate: u32, data: bool) {
        if !data {
            if r.ok() { self.mgmt_ok += 1; } else { self.mgmt_fail += 1; }
            return;
        }
        let tries = (r.failure_frame as usize).min(15);
        if r.ok() {
            self.ok += 1;
            self.tries_ok[tries] += 1;
            self.rate_ok[rate_bucket(final_rate)] += 1;
        } else {
            self.fail_fw += 1;
            self.tries_fail[tries] += 1;
            self.rate_fail[rate_bucket(final_rate)] += 1;
        }
        self.status[status_bucket(r.status)] += 1;
        self.bt_kill += r.bt_kill_count as u64;
        if r.bt_kill_count > 0 { self.bt_kill_frames += 1; }
        self.rts_fail += r.failure_rts as u64;
        if r.reduced_tpc > 0 { self.tpc_reduced += 1; }
    }

    /// Every failure before or after the firmware (the old `tx_failed`).
    pub fn failed_total(&self) -> u64 { self.fail_fw + self.busy + self.notready + self.capped }

    /// Counters since `prev`.
    pub fn delta(&self, prev: &TxStats) -> TxStats {
        TxStats {
            enq: self.enq.saturating_sub(prev.enq), ok: self.ok.saturating_sub(prev.ok),
            fail_fw: self.fail_fw.saturating_sub(prev.fail_fw), busy: self.busy.saturating_sub(prev.busy),
            notready: self.notready.saturating_sub(prev.notready), capped: self.capped.saturating_sub(prev.capped),
            status: sub_arr(&self.status, &prev.status),
            tries_ok: sub_arr(&self.tries_ok, &prev.tries_ok), tries_fail: sub_arr(&self.tries_fail, &prev.tries_fail),
            rate_ok: sub_arr(&self.rate_ok, &prev.rate_ok), rate_fail: sub_arr(&self.rate_fail, &prev.rate_fail),
            bt_kill: self.bt_kill.saturating_sub(prev.bt_kill), bt_kill_frames: self.bt_kill_frames.saturating_sub(prev.bt_kill_frames),
            rts_fail: self.rts_fail.saturating_sub(prev.rts_fail), tpc_reduced: self.tpc_reduced.saturating_sub(prev.tpc_reduced),
            mgmt_ok: self.mgmt_ok.saturating_sub(prev.mgmt_ok), mgmt_fail: self.mgmt_fail.saturating_sub(prev.mgmt_fail),
        }
    }

    /// The counters on one line (non-zero histogram buckets only).
    pub fn line(&self) -> String {
        let mut s = format!("enq {} ok {} fail_fw {} busy {} notready {} capped {}", self.enq, self.ok, self.fail_fw,
                            self.busy, self.notready, self.capped);
        let done = self.ok + self.fail_fw;
        if done > 0 { let _ = write!(s, " ({}% fail)", self.fail_fw * 100 / done); }
        s.push_str("; status");
        hist(&mut s, &self.status, |i| String::from(STATUS_NAMES[i]));
        s.push_str("; retries(ok)");
        hist(&mut s, &self.tries_ok, |i| format!("{}{}", i, if i == 15 { "+" } else { "" }));
        s.push_str("; retries(fail)");
        hist(&mut s, &self.tries_fail, |i| format!("{}{}", i, if i == 15 { "+" } else { "" }));
        s.push_str("; rate(ok)");
        hist(&mut s, &self.rate_ok, rate_label);
        s.push_str("; rate(fail)");
        hist(&mut s, &self.rate_fail, rate_label);
        let _ = write!(s, "; bt_kill {} ({} frames) rts_fail {} tpc_reduced {} mgmt ok/fail {}/{}",
                       self.bt_kill, self.bt_kill_frames, self.rts_fail, self.tpc_reduced, self.mgmt_ok, self.mgmt_fail);
        s
    }

    /// Full detail, one `key: value` line per counter group.
    pub fn detail(&self, out: &mut String) {
        let _ = writeln!(out, "tx: enq {} ok {} fail_fw {} busy {} notready {} capped {}", self.enq, self.ok,
                         self.fail_fw, self.busy, self.notready, self.capped);
        for (i, n) in self.status.iter().enumerate() {
            let _ = writeln!(out, "tx_status {:>5}: {:>8}  {}", STATUS_NAMES[i], n, STATUS_MEANING[i]);
        }
        let mut l = String::from("tx_retries ok:  ");
        for n in self.tries_ok.iter() { let _ = write!(l, " {}", n); }
        let _ = writeln!(out, "{}", l);
        let mut l = String::from("tx_retries fail:");
        for n in self.tries_fail.iter() { let _ = write!(l, " {}", n); }
        let _ = writeln!(out, "{}", l);
        let mut l = String::from("tx_rate ok:  ");
        for (i, n) in self.rate_ok.iter().enumerate() { let _ = write!(l, " {}={}", rate_label(i), n); }
        let _ = writeln!(out, "{}", l);
        let mut l = String::from("tx_rate fail:");
        for (i, n) in self.rate_fail.iter().enumerate() { let _ = write!(l, " {}={}", rate_label(i), n); }
        let _ = writeln!(out, "{}", l);
        let _ = writeln!(out, "tx_coex: bt_kill {} (frames {}) rts_fail {} tpc_reduced {}", self.bt_kill,
                         self.bt_kill_frames, self.rts_fail, self.tpc_reduced);
        let _ = writeln!(out, "tx_mgmt: ok {} fail {}", self.mgmt_ok, self.mgmt_fail);
    }
}

/// A rate_bucket histogram as ` 1M:n 2M:n ...` (non-zero buckets).
pub fn rate_hist_line(h: &[u64]) -> String {
    let mut s = String::new();
    hist(&mut s, h, rate_label);
    s
}

fn rate_label(i: usize) -> String {
    match LEGACY_R500K.get(i) { Some(&r) => format!("{}M", mbps(r)), None => String::from("other") }
}

fn hist(s: &mut String, h: &[u64], label: impl Fn(usize) -> String) {
    let mut any = false;
    for (i, &n) in h.iter().enumerate() {
        if n > 0 { let _ = write!(s, " {}:{}", label(i), n); any = true; }
    }
    if !any { s.push_str(" -"); }
}
