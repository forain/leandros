//! TX rate control for an iwlwifi station (legacy, and HT at 20 MHz), and
//! the TX statistics the driver keeps. Hardware independent and host tested
//! (drivers/iwlwifi-tests).
//!
//! Rate control is a reduced form of Linux iwlwifi `mvm/rs.c`, one column
//! per antenna. The rates form one ladder ordered by expected throughput:
//! the legacy rates of the AP, or with HT the legacy rates slower than MCS 0
//! followed by the single-stream MCS 0-7 (short GI with iwlwifi.txsgi=1)
//! and, with iwlwifi.txmimo=1, the two-stream MCS that beat MCS 7. Linux's
//! separate legacy / SISO / MIMO columns become steps of that ladder: the
//! retry chain falls from HT into legacy the way rs_build_rates_table's
//! secondary column does.
//! * initial rate from the beacon RSSI (rs_get_initial_rate's legacy and HT tables);
//! * per-rate success ratio from every TX response: each attempt of the
//!   retry chain (failure_frame + 1 of them, read off the table the firmware
//!   had) counts a failure, the last one the frame's verdict
//!   (rs_collect_tlc_data); kept as an EWMA plus the counts that make a
//!   window "valid" (8 successes or 3 failures);
//! * the move up / down / stay of rs_get_rate_action on expected throughput
//!   (success ratio x Linux's expected_tpt_legacy / _siso_20MHz /
//!   _mimo2_20MHz, the no-aggregation columns: we do not aggregate TX);
//! * the station table (LQ_CMD) starts at the chosen rate: two tries there
//!   (and at the next two lower HT rates), one at each lower rate down to
//!   the lowest basic rate, which fills the rest; it is resent at most every
//!   100 ms;
//! * antenna A/B toggle (when both are valid and none is forced) after a
//!   run of frames that failed outright, or when even the lowest rate fails;
//! * STBC mode (no toggle / probe): the legacy retries alternate antennas,
//!   and the one delivering clearly more legacy attempts starts them;
//! * antenna comparison while the link is healthy (the antenna "column" of
//!   rs.c's search): every `ANT_PROBE_MS` (sooner when the other chain
//!   hears the AP's beacons `ANT_HINT_DB` stronger) the table moves to the
//!   other antenna at the same rate for at least `ANT_PROBE_LEN_MS` and a
//!   valid window; it stays there only when its throughput (success x
//!   expected) beats the old antenna's by `ANT_KEEP_PCT` %, else the old
//!   antenna, rate and windows come back;
//! * a stale ladder (no TX for `STALE_MS`) restarts, at the first TX
//!   outcome after the idle period, from the rate the current beacon signal
//!   suggests (`on_signal`), so a laptop that moved while idle does not
//!   resume at a rate from before.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::iwl_assoc::{self as asc, TxResp, ANT_A, ANT_AB, ANT_B, LEGACY_R500K, RATE_ID_MSK};

/// Linux rs.c expected_tpt_legacy, in the order of `LEGACY_R500K`.
pub const EXPECTED_TPT: [u32; 12] = [7, 13, 35, 40, 57, 58, 72, 98, 121, 154, 177, 186];
/// Linux rs.c expected_tpt_siso_20MHz / _mimo2_20MHz, MCS 0-7 of the
/// stream count, [long GI, short GI], without aggregation.
pub const EXPECTED_TPT_SISO: [[u32; 8]; 2] = [[42, 76, 102, 124, 159, 183, 193, 202], [46, 82, 110, 132, 168, 192, 202, 210]];
pub const EXPECTED_TPT_MIMO2: [[u32; 8]; 2] = [[74, 123, 155, 179, 213, 235, 243, 250], [81, 131, 164, 187, 221, 242, 250, 256]];
/// The same with aggregation (rs.c columns 2 / 3: AGG+NGI, AGG+SGI).
pub const EXPECTED_TPT_SISO_AGG: [[u32; 8]; 2] = [[49, 97, 145, 192, 285, 375, 420, 464], [54, 108, 160, 213, 315, 415, 465, 513]];
pub const EXPECTED_TPT_MIMO2_AGG: [[u32; 8]; 2] = [[98, 193, 286, 375, 550, 718, 799, 878], [109, 214, 316, 414, 607, 790, 879, 965]];
/// Nominal 20 MHz long-GI rates of MCS 0-7 (one stream) in 500 kb/s units.
pub const HT_R500K: [u32; 8] = [13, 26, 39, 52, 78, 104, 117, 130];

/// A ladder entry is a rate id: a legacy rate in 500 kb/s units (2..108),
/// or HT_ID | MCS (0-15), with HT_SGI for the short guard interval.
pub const HT_ID: u8 = 0x80;
pub const HT_SGI: u8 = 0x40;
/// 40 MHz (the MCS index uses the low 4 bits).
pub const HT_40: u8 = 0x20;
/// Linux rs.c expected_tpt_siso_40MHz / _mimo2_40MHz, MCS 0-7, [NGI, SGI,
/// AGG+NGI, AGG+SGI].
pub const EXPECTED_TPT_SISO40: [[u32; 8]; 4] = [[77, 127, 160, 184, 220, 242, 250, 257], [83, 135, 169, 193, 229, 250, 257, 264],
                                                 [101, 199, 295, 389, 570, 744, 828, 911], [112, 220, 326, 429, 629, 819, 912, 1000]];
pub const EXPECTED_TPT_MIMO2_40: [[u32; 8]; 4] = [[123, 182, 214, 235, 264, 279, 285, 289], [131, 191, 222, 242, 270, 284, 289, 293],
                                                   [200, 390, 571, 741, 1067, 1365, 1505, 1640], [221, 430, 630, 816, 1169, 1490, 1641, 1784]];
/// Nominal 40 MHz long-GI rates of MCS 0-7 (one stream) in 500 kb/s units.
pub const HT40_R500K: [u32; 8] = [27, 54, 81, 108, 162, 216, 243, 270];

pub fn is_ht(id: u8) -> bool { id & HT_ID != 0 }
pub fn ht_id(mcs: u8, sgi: bool) -> u8 { HT_ID | (mcs & 0x0f) | if sgi { HT_SGI } else { 0 } }
pub fn ht_id_w(mcs: u8, sgi: bool, ht40: bool) -> u8 { ht_id(mcs, sgi) | if ht40 { HT_40 } else { 0 } }
pub fn is_40(id: u8) -> bool { is_ht(id) && id & HT_40 != 0 }

/// Expected throughput of a rate id with TX aggregation (rs.c AGG
/// columns; legacy rates are never aggregated).
pub fn expected_tpt_agg(id: u8) -> u32 {
    if is_40(id) {
        let (mcs, sgi) = ((id & 0x0f) as usize, (id & HT_SGI != 0) as usize);
        return match mcs { 0..=7 => EXPECTED_TPT_SISO40[2 + sgi][mcs], 8..=15 => EXPECTED_TPT_MIMO2_40[2 + sgi][mcs - 8], _ => 0 };
    }
    if is_ht(id) {
        let (mcs, sgi) = ((id & 0x0f) as usize, (id & HT_SGI != 0) as usize);
        match mcs { 0..=7 => EXPECTED_TPT_SISO_AGG[sgi][mcs], 8..=15 => EXPECTED_TPT_MIMO2_AGG[sgi][mcs - 8], _ => 0 }
    } else {
        expected_tpt(id)
    }
}

/// Expected throughput of a rate id (rs.c tables, no aggregation).
pub fn expected_tpt(id: u8) -> u32 {
    if is_40(id) {
        let (mcs, sgi) = ((id & 0x0f) as usize, (id & HT_SGI != 0) as usize);
        return match mcs { 0..=7 => EXPECTED_TPT_SISO40[sgi][mcs], 8..=15 => EXPECTED_TPT_MIMO2_40[sgi][mcs - 8], _ => 0 };
    }
    if is_ht(id) {
        let (mcs, sgi) = ((id & 0x0f) as usize, (id & HT_SGI != 0) as usize);
        match mcs { 0..=7 => EXPECTED_TPT_SISO[sgi][mcs], 8..=15 => EXPECTED_TPT_MIMO2[sgi][mcs - 8], _ => 0 }
    } else {
        legacy_index(id).map_or(0, |i| EXPECTED_TPT[i])
    }
}

/// Nominal rate of a rate id in 500 kb/s units (the iwlwifi.maxrate cap).
pub fn nominal_r500k(id: u8) -> u32 {
    if is_ht(id) {
        let mcs = (id & 0x0f) as usize;
        (if is_40(id) { HT40_R500K[mcs % 8] } else { HT_R500K[mcs % 8] }) * (1 + (mcs / 8) as u32)
    } else {
        id as u32
    }
}

/// The rate_n_flags of a rate id; single-stream rates on antenna `ant`,
/// two-stream ones on both.
pub fn rate_code(id: u8, ant: u8) -> u32 {
    if is_ht(id) {
        let mcs = id & 0x0f;
        asc::ht_rate_w(mcs, id & HT_SGI != 0, if mcs >= 8 { ANT_AB } else { ant }, is_40(id))
    } else {
        asc::legacy_rate_ant(id, ant).unwrap_or(0)
    }
}

/// The rate id a rate_n_flags carries (any antenna).
pub fn rate_id(rate_n_flags: u32) -> Option<u8> {
    match asc::rate_mcs(rate_n_flags) {
        Some((mcs, sgi)) if mcs < 16 => Some(ht_id_w(mcs, sgi, asc::rate_is_40(rate_n_flags))),
        Some(_) => None,
        None => asc::rate_r500k(rate_n_flags),
    }
}

/// A rate id as text: "54M", "5.5M", "MCS7", "MCS7s" (short GI).
pub fn rate_label(id: u8) -> String {
    if is_ht(id) {
        format!("MCS{}{}{}", id & 0x0f, if is_40(id) { "/40" } else { "" }, if id & HT_SGI != 0 { "s" } else { "" })
    } else {
        format!("{}M", mbps(id))
    }
}

/// Linux rs_optimal_rates_ht: the single-stream MCS worth trying first at a
/// beacon signal of `rssi` dBm.
pub fn initial_mcs(rssi: i8) -> u8 {
    let t: [(i8, u8); 8] = [(-60, 7), (-64, 6), (-68, 5), (-72, 4), (-80, 3), (-84, 2), (-85, 1), (i8::MIN, 0)];
    t.iter().find(|&&(th, _)| rssi >= th).map_or(0, |&(_, m)| m)
}
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
/// STBC mode, legacy retries alternating antennas: attempts per antenna
/// before their delivery ratios are compared, and the lead (percentage
/// points) at which the other antenna starts the legacy chain.
pub const LEG_ANT_MIN: u32 = 128;
pub const LEG_ANT_GAIN: u32 = 20;
/// Antenna comparison: period, minimum probe length, the extra time a
/// probe may wait for a valid window, the gain that keeps the new antenna,
/// and the beacon-energy difference that brings the next probe forward.
pub const ANT_PROBE_MS: u64 = 30_000;
pub const ANT_PROBE_LEN_MS: u64 = 1000;
pub const ANT_PROBE_MAX_MS: u64 = 3000;
pub const ANT_KEEP_PCT: u32 = 125;
pub const ANT_HINT_DB: i16 = 6;
pub const ANT_HINT_MIN_MS: u64 = 5000;

fn legacy_index(r500k: u8) -> Option<usize> {
    if is_ht(r500k) { return None; }
    LEGACY_R500K.iter().position(|&r| r == r500k & 0x7f)
}

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
    /// Success ratio, permille, over the last `RATE_WINDOW` attempts.
    pub sr: u32,
    pub last_ms: u64,
    /// One bit per attempt in the window, newest in bit 0 (1 = delivered).
    pub data: u64,
}

/// Attempts a rate's window holds (Linux IWL_RATE_MAX_WINDOW). The success
/// ratio is the plain ratio over the window: an EWMA over single attempts
/// is order dependent, and the failed subframes of an A-MPDU, recorded
/// after its delivered ones, would dominate it (25 sent, 19 acked: EWMA
/// 1/8 gives 45 %, not 76 %), which pushed the rate down under load.
pub const RATE_WINDOW: u32 = 62;

impl RateWin {
    pub fn record(&mut self, ok: bool, now: u64) {
        if self.att > 0 && now.saturating_sub(self.last_ms) > STALE_MS { *self = RateWin::default(); }
        // rs.c _rs_collect_tx_data: drop the oldest attempt from a full window.
        if self.att >= RATE_WINDOW {
            self.att = RATE_WINDOW - 1;
            let oldest = 1u64 << (RATE_WINDOW - 1);
            if self.data & oldest != 0 { self.data &= !oldest; self.succ -= 1; }
        }
        self.att += 1;
        self.data <<= 1;
        if ok { self.succ += 1; self.data |= 1; }
        self.sr = self.succ * 1000 / self.att;
        self.last_ms = now;
    }
    pub fn fresh(&self, now: u64) -> bool { self.att > 0 && now.saturating_sub(self.last_ms) <= STALE_MS }
    /// Enough attempts in the window for a judgment (Linux: 8 successes or 3 failures).
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
    /// HT association: rate control over MCS too.
    pub ht: Option<HtRates>,
}

/// The HT rates rate control may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HtRates {
    /// Short guard interval (the AP receives SGI at 20 MHz).
    pub sgi: bool,
    /// Two-stream MCS 8-15 (the AP receives two streams, both TX chains).
    pub mimo: bool,
    /// Transmit at 40 MHz (the link runs 40 MHz and the AP takes it).
    pub ht40: bool,
    /// Single-stream MCS go out with STBC on both antennas (the AP receives
    /// STBC, we have two TX chains: rs.c `stbc_capable` + the rate flag of
    /// firmware without LQ_SS_PARAMS); the legacy part of the retry chain
    /// then alternates antennas (rs_fill_rates_for_column's toggle_ant).
    pub stbc: bool,
    /// HT rates use LDPC coding (the AP receives LDPC: rs.c `lq_sta->ldpc`).
    pub ldpc: bool,
}

/// Rate control for one station.
#[derive(Clone, Debug)]
pub struct RateCtl {
    /// Usable rates (rate ids: legacy in 500 kb/s units, or HT_ID | MCS),
    /// slowest first by expected throughput.
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
    /// TX aggregation is on: expected throughput from the AGG columns, and
    /// the LQ's A-MPDU frame limit.
    pub agg: bool,
    pub agg_limit: u8,
    /// A-MPDU outcomes (BA notifications) counted.
    pub ampdus: u64,
    /// The antenna comparison in progress, when the next one is due (0 =
    /// not yet scheduled), and its counters.
    pub probe: Option<AntProbe>,
    pub next_probe_ms: u64,
    pub probes: u64,
    pub probes_kept: u64,
    /// Rate restarts from the beacon signal after an idle period.
    pub reseeds: u64,
    /// STBC mode: legacy attempts and deliveries per antenna (A, B) since
    /// the last comparison (the alternating chain tries both).
    pub leg_att: [u32; 2],
    pub leg_ok: [u32; 2],
    /// The last beacon signal (dBm).
    pub signal: Option<i8>,
    band_24: bool,
    ht_cfg: Option<HtRates>,
}

/// What an antenna probe goes back to when the other antenna is not better.
#[derive(Clone, Debug)]
pub struct AntProbe {
    pub from_ant: u8,
    pub from_cur: usize,
    pub from_win: Vec<RateWin>,
    /// The old antenna's throughput (success permille x expected) at the start.
    pub old_tpt: u32,
    pub until_ms: u64,
}

impl RateCtl {
    pub fn new(cfg: &RateCfg) -> RateCtl {
        let mut rates: Vec<u8> = cfg.ap_rates.iter().map(|r| r & 0x7f)
            .filter(|&r| legacy_index(r).is_some() && (cfg.max_r500k == 0 || r <= cfg.max_r500k)).collect();
        rates.sort_unstable_by_key(|&r| legacy_index(r));
        rates.dedup();
        if rates.is_empty() { rates.push(if cfg.band_24 { 2 } else { 12 }); }
        let mut want_ht = None;
        if let Some(h) = cfg.ht {
            // HT: the legacy rates below MCS 0, then the MCS ladder.
            let cap = |id: u8| cfg.max_r500k == 0 || nominal_r500k(id) <= cfg.max_r500k as u32;
            let mut ht: Vec<u8> = (0..8).map(|m| ht_id_w(m, h.sgi, h.ht40)).filter(|&id| cap(id)).collect();
            if h.mimo {
                let top = ht.last().map_or(0, |&id| expected_tpt(id));
                ht.extend((8..16).map(|m| ht_id_w(m, h.sgi, h.ht40)).filter(|&id| cap(id) && expected_tpt(id) > top));
            }
            if let Some(&low) = ht.first() {
                rates.retain(|&r| expected_tpt(r) < expected_tpt(low));
                rates.extend_from_slice(&ht);
                // Unknown signal: MCS 3, the middle of the table.
                // 40 MHz spreads the power over twice the band: 3 dB less per subcarrier.
                want_ht = Some(ht_id_w(cfg.rssi.map_or(3, |r| initial_mcs(if h.ht40 { r.saturating_sub(3) } else { r })), h.sgi, h.ht40));
            }
        }
        let floor = cfg.ap_rates.iter().filter(|r| *r & 0x80 != 0)
            .filter_map(|r| rates.iter().position(|&x| x == r & 0x7f)).min().unwrap_or(0);
        let cur = match want_ht {
            Some(w) => rates.iter().rposition(|&r| expected_tpt(r) <= expected_tpt(w)).unwrap_or(0),
            None => {
                // Unknown signal: 24 Mb/s, the middle of the table.
                let want = cfg.rssi.map_or(48, |s| initial_r500k(s, cfg.band_24));
                rates.iter().rposition(|&r| legacy_index(r) <= legacy_index(want)).unwrap_or(0)
            }
        };
        let both = cfg.valid_ant & (ANT_A | ANT_B) == ANT_A | ANT_B;
        let (ant, ant_fixed) = match cfg.fixed_ant {
            Some(a) if cfg.valid_ant & a != 0 => (a, true),
            _ => (if cfg.valid_ant & ANT_A != 0 || cfg.valid_ant == 0 { ANT_A } else { ANT_B }, !both),
        };
        let n = rates.len();
        RateCtl { rates, floor, win: alloc::vec![RateWin::default(); n], cur, ant, valid_ant: cfg.valid_ant, ant_fixed,
                  sent: None, sent_ms: 0, pending: true, fail_streak: 0, last_toggle_ms: 0,
                  ups: 0, downs: 0, toggles: 0, lq_sent: 0, mismatched: 0, agg: false, agg_limit: 63, ampdus: 0,
                  probe: None, next_probe_ms: 0, probes: 0, probes_kept: 0, reseeds: 0, leg_att: [0; 2], leg_ok: [0; 2], signal: None, band_24: cfg.band_24, ht_cfg: cfg.ht }
    }

    pub fn cur_r500k(&self) -> u8 { self.rates[self.cur] }

    /// The retry chain for the current rate and antenna: two tries at the
    /// current rate (and at the two HT rates below it), one at each lower
    /// rate down to the lowest basic rate, which fills the rest.
    pub fn table(&self) -> [u32; 16] {
        let floor = self.floor.min(self.cur);
        // Legacy entries alternate antennas in STBC mode (the HT ones use both).
        let toggle = self.legacy_toggle();
        let mut lant = self.ant;
        let mut code = |i: usize| {
            if is_ht(self.rates[i]) || !toggle { return self.code(i, self.ant); }
            let a = lant;
            lant = if lant == ANT_A { ANT_B } else { ANT_A };
            self.code(i, a)
        };
        let mut t = [0u32; 16];
        let mut n = 0;
        t[n] = code(self.cur); n += 1;
        t[n] = code(self.cur); n += 1;
        let mut i = self.cur;
        while i > floor && n < 16 {
            i -= 1;
            let tries = if is_ht(self.rates[i]) && self.cur - i < 3 { 2 } else { 1 };
            for _ in 0..tries { if n < 16 { t[n] = code(i); n += 1; } }
        }
        while n < 16 { t[n] = code(floor); n += 1; }
        t
    }

    /// The rate_n_flags of ladder entry `i` on antenna `ant` (single-stream
    /// MCS with STBC go on both antennas; LDPC on every HT rate).
    pub fn code(&self, i: usize, ant: u8) -> u32 {
        let id = self.rates[i];
        match self.ht_cfg {
            Some(h) if is_ht(id) => {
                let mut c = if h.stbc && id & 0x0f < 8 { rate_code(id, ANT_AB) | asc::RATE_MCS_STBC } else { rate_code(id, ant) };
                if h.ldpc { c |= asc::RATE_MCS_LDPC_V1; }
                c
            }
            _ => rate_code(id, ant),
        }
    }

    /// STBC mode: HT rates on both antennas, the legacy fallback alternating
    /// (unless one antenna is forced or the only valid one).
    fn legacy_toggle(&self) -> bool { self.stbc_mode() && !self.ant_fixed }

    fn stbc_mode(&self) -> bool { self.ht_cfg.map_or(false, |h| h.stbc) }

    /// The first single-stream entry of `table` (0 without MIMO entries).
    pub fn mimo_delim(t: &[u32; 16]) -> u8 {
        if !matches!(asc::rate_mcs(t[0]), Some((m, _)) if m >= 8) { return 0; }
        t.iter().position(|&r| !matches!(asc::rate_mcs(r), Some((m, _)) if m >= 8)).unwrap_or(16) as u8
    }

    pub fn lq(&self, sta_id: u8) -> Vec<u8> {
        let t = self.table();
        asc::lq_cmd_full(sta_id, &t, self.ant, RateCtl::mimo_delim(&t), self.agg_limit)
    }

    /// The current rate as text ("MCS4", "54M").
    pub fn cur_label(&self) -> String { rate_label(self.rates[self.cur]) }

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
        let r = rate_id(rate_n_flags)?;
        self.rates.iter().position(|&x| x == r)
    }

    fn can_toggle(&self, now: u64) -> bool {
        !self.ant_fixed && !self.stbc_mode() && self.valid_ant & (ANT_A | ANT_B) == ANT_A | ANT_B
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
        if self.reseed_if_idle(now) { return Action::Stay; }
        if t[0] & RATE_ID_MSK != r.initial_rate & RATE_ID_MSK { self.mismatched += 1; return Action::Stay; }
        let ok = r.ok();
        if !ok && !r.air_fail() { return Action::Stay; }
        let tries = (r.failure_frame as usize).min(15);
        for (i, &rate) in t.iter().enumerate().take(tries + 1) {
            if let Some(idx) = self.index_of(rate) { self.win[idx].record(i == tries && ok, now); }
        }
        if ok { self.fail_streak = 0; } else { self.fail_streak += 1; }
        if self.legacy_toggle() && self.legacy_ant_check(&t, tries, ok, now) { return Action::Stay; }
        self.decide(now)
    }

    /// STBC mode: the legacy part of the chain alternates antennas starting
    /// with `self.ant`, and the antenna toggle / probe are off. When one
    /// antenna is much worse (laptop position, a loose antenna lead), every
    /// legacy frame first wastes an attempt on it: the retry histogram then
    /// shows deliveries at odd retry counts. Count legacy attempts per
    /// antenna (the chain is known, so each attempt's antenna is too) and
    /// let the clearly better antenna start the legacy chain (rs.c explores
    /// the LEGACY_ANT_A / _B columns for the same reason). True when the
    /// table changed.
    fn legacy_ant_check(&mut self, t: &[u32; 16], tries: usize, ok: bool, now: u64) -> bool {
        for (i, &r) in t.iter().enumerate().take(tries + 1) {
            if rate_id(r).map_or(true, is_ht) { continue; }
            let k = match ((r & asc::RATE_MCS_ANT_MSK) >> asc::RATE_MCS_ANT_POS) as u8 { ANT_A => 0, ANT_B => 1, _ => continue };
            self.leg_att[k] += 1;
            if i == tries && ok { self.leg_ok[k] += 1; }
        }
        let mine = if self.ant == ANT_B { 1 } else { 0 };
        let other = 1 - mine;
        if self.leg_att[mine] < LEG_ANT_MIN { return false; }
        if self.leg_att[other] < LEG_ANT_MIN {
            // The first antenna delivers: the other is hardly tried. Start over now and then.
            if self.leg_att[mine] >= 16 * LEG_ANT_MIN { self.leg_att = [0; 2]; self.leg_ok = [0; 2]; }
            return false;
        }
        let sr = |k: usize| self.leg_ok[k] * 100 / self.leg_att[k];
        let flip = sr(other) >= sr(mine) + LEG_ANT_GAIN
            && (self.toggles == 0 || now.saturating_sub(self.last_toggle_ms) >= TOGGLE_MIN_MS);
        self.leg_att = [0; 2];
        self.leg_ok = [0; 2];
        if !flip { return false; }
        self.ant = if self.ant == ANT_A { ANT_B } else { ANT_A };
        self.toggles += 1;
        self.last_toggle_ms = now;
        self.pending = true;
        true
    }

    /// Expected throughput of ladder entry `i` (with or without A-MPDU).
    pub fn etpt(&self, i: usize) -> u32 {
        if self.agg { expected_tpt_agg(self.rates[i]) } else { expected_tpt(self.rates[i]) }
    }

    fn tpt(&self, i: usize, now: u64) -> Option<u32> {
        let w = &self.win[i];
        if w.valid(now) { Some(w.sr * self.etpt(i)) } else { None }
    }

    /// TX aggregation switched on (window `limit`) or off: the LQ changes.
    pub fn set_agg(&mut self, on: bool, limit: u8) {
        if self.agg != on || self.agg_limit != limit {
            self.agg = on;
            self.agg_limit = limit;
            self.pending = true;
        }
    }

    /// One A-MPDU's outcome (BA notification): `txed` subframes sent at
    /// `rate_n_flags` (the A-MPDU's TX response), `acked` of them
    /// acknowledged; rs_collect_tlc_data with ampdu_len / ampdu_ack_len.
    pub fn on_ampdu(&mut self, rate_n_flags: u32, txed: u8, acked: u8, now: u64) -> Action {
        let t = match self.sent { Some(t) => t, None => return Action::Stay };
        if txed == 0 { return Action::Stay; }
        if self.reseed_if_idle(now) { return Action::Stay; }
        if t[0] & RATE_ID_MSK != rate_n_flags & RATE_ID_MSK { self.mismatched += 1; return Action::Stay; }
        let idx = match self.index_of(rate_n_flags) { Some(i) => i, None => return Action::Stay };
        self.ampdus += 1;
        let acked = acked.min(txed);
        for k in 0..txed { self.win[idx].record(k < acked, now); }
        if acked > 0 { self.fail_streak = 0; } else { self.fail_streak += 1; }
        self.decide(now)
    }

    fn both_ants(&self) -> bool { !self.ant_fixed && !self.stbc_mode() && self.valid_ant & (ANT_A | ANT_B) == ANT_A | ANT_B }

    fn start_probe(&mut self, now: u64) {
        let c = self.cur;
        let old_tpt = self.win[c].sr * self.etpt(c);
        self.probe = Some(AntProbe { from_ant: self.ant, from_cur: c, from_win: self.win.clone(), old_tpt, until_ms: now + ANT_PROBE_LEN_MS });
        self.ant = if self.ant == ANT_A { ANT_B } else { ANT_A };
        for w in self.win.iter_mut() { *w = RateWin::default(); }
        self.fail_streak = 0;
        self.probes += 1;
        self.pending = true;
    }

    fn end_probe(&mut self, now: u64, keep: bool) {
        let p = match self.probe.take() { Some(p) => p, None => return };
        self.next_probe_ms = now + ANT_PROBE_MS;
        if keep {
            self.probes_kept += 1;
            return;
        }
        self.ant = p.from_ant;
        self.cur = p.from_cur;
        self.win = p.from_win;
        // The old windows aged during the probe; keep them fresh.
        for w in self.win.iter_mut() { if w.att > 0 { w.last_ms = now; } }
        self.fail_streak = 0;
        self.pending = true;
    }

    /// The probe in progress or a new one; true when the table changed.
    fn antenna_search(&mut self, now: u64) -> bool {
        if let Some(p) = self.probe.as_ref() {
            let (until, old) = (p.until_ms, p.old_tpt);
            if self.fail_streak >= FAIL_STREAK_TOGGLE { self.end_probe(now, false); return true; }
            let w = self.win[self.cur];
            if (now >= until && w.valid(now)) || now >= until + ANT_PROBE_MAX_MS {
                let new = if w.valid(now) { w.sr * self.etpt(self.cur) } else { 0 };
                self.end_probe(now, new as u64 * 100 >= old as u64 * ANT_KEEP_PCT as u64);
                return true;
            }
            return false;
        }
        if !self.both_ants() { return false; }
        if self.next_probe_ms == 0 { self.next_probe_ms = now + ANT_PROBE_MS; return false; }
        if now >= self.next_probe_ms && self.win[self.cur].valid(now) {
            self.start_probe(now);
            return true;
        }
        false
    }

    /// Beacon energy per chain (dBm): when the other antenna hears the AP
    /// clearly better, compare antennas soon.
    pub fn chain_hint(&mut self, a_dbm: i16, b_dbm: i16, now: u64) {
        if !self.both_ants() || self.probe.is_some() { return; }
        let (mine, other) = if self.ant == ANT_A { (a_dbm, b_dbm) } else { (b_dbm, a_dbm) };
        if other - mine >= ANT_HINT_DB {
            let soonest = self.next_probe_ms.saturating_sub(ANT_PROBE_MS) + ANT_HINT_MIN_MS;
            self.next_probe_ms = self.next_probe_ms.min(soonest.max(now));
        }
    }

    /// The tracked AP's beacon signal (dBm), for the restart after idle.
    pub fn on_signal(&mut self, dbm: i8) { self.signal = Some(dbm); }

    /// The first TX outcome after an idle period (every window stale): the
    /// ladder restarts at the rate the current beacon signal suggests.
    /// True when the table changed (the outcome is then not counted).
    fn reseed_if_idle(&mut self, now: u64) -> bool {
        let dbm = match self.signal { Some(d) => d, None => return false };
        if self.probe.is_some() || self.win.iter().any(|w| w.fresh(now)) { return false; }
        let want = match self.ht_cfg.filter(|_| self.rates.iter().any(|&r| is_ht(r))) {
            Some(h) => {
                let id = ht_id_w(initial_mcs(if h.ht40 { dbm.saturating_sub(3) } else { dbm }), h.sgi, h.ht40);
                self.rates.iter().rposition(|&r| expected_tpt(r) <= expected_tpt(id)).unwrap_or(0)
            }
            None => {
                let r = initial_r500k(dbm, self.band_24);
                self.rates.iter().rposition(|&x| legacy_index(x) <= legacy_index(r)).unwrap_or(0)
            }
        };
        if want == self.cur { return false; }
        self.cur = want;
        self.reseeds += 1;
        self.pending = true;
        true
    }

    fn decide(&mut self, now: u64) -> Action {
        // One step per table that reached the firmware.
        if self.pending { return Action::Stay; }
        if self.antenna_search(now) { return Action::Stay; }
        if self.probe.is_none() && self.fail_streak >= FAIL_STREAK_TOGGLE && self.can_toggle(now) {
            self.toggle(now);
            return Action::Stay;
        }
        let c = self.cur;
        if !self.win[c].valid(now) { return Action::Stay; }
        let sr = self.win[c].sr;
        let cur_tpt = sr * self.etpt(c);
        let low = if c > 0 { Some(self.tpt(c - 1, now)) } else { None };
        let high = if c + 1 < self.rates.len() { Some(self.tpt(c + 1, now)) } else { None };
        let low_expected = if c > 0 { self.etpt(c - 1) } else { 0 };
        let a = rate_action(sr, cur_tpt, low, high, low_expected);
        match a {
            Action::Up if high.is_some() => { self.cur += 1; self.ups += 1; self.pending = true; }
            Action::Down if c > 0 => { self.cur -= 1; self.downs += 1; self.pending = true; }
            Action::Down => {
                // Already at the slowest rate and it fails: the other antenna.
                if sr <= SR_FORCE_DECREASE && self.probe.is_none() && self.can_toggle(now) { self.toggle(now); }
                return Action::Stay;
            }
            _ => return Action::Stay,
        }
        a
    }

    /// One line: current rate, antenna, per-rate success.
    pub fn summary(&self, now: u64) -> String {
        let mut s = format!("rc {} ant {} up {} down {} toggles {} lq {} mismatched {} ampdus {} probes {} kept {} reseeds {}; sr",
                            self.cur_label(), if self.ant == ANT_B { "B" } else { "A" },
                            self.ups, self.downs, self.toggles, self.lq_sent, self.mismatched, self.ampdus,
                            self.probes, self.probes_kept, self.reseeds);
        for (i, w) in self.win.iter().enumerate() {
            if w.fresh(now) { let _ = write!(s, " {}:{}%/{}", rate_label(self.rates[i]), w.sr / 10, w.att); }
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

/// Rate histogram buckets: the 12 legacy rates (LEGACY_R500K order), HT
/// MCS 0-15 (either guard interval), other.
pub const RATE_BUCKETS: usize = 29;

pub fn rate_bucket(rate_n_flags: u32) -> usize {
    match asc::rate_mcs(rate_n_flags) {
        Some((m, _)) if m < 16 => 12 + m as usize,
        Some(_) => 28,
        None => asc::rate_r500k(rate_n_flags).and_then(legacy_index).unwrap_or(28),
    }
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
    pub rate_ok: [u64; RATE_BUCKETS],
    pub rate_fail: [u64; RATE_BUCKETS],
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
        hist(&mut s, &self.rate_ok, bucket_label);
        s.push_str("; rate(fail)");
        hist(&mut s, &self.rate_fail, bucket_label);
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
        for (i, n) in self.rate_ok.iter().enumerate() { if *n > 0 || i < 12 { let _ = write!(l, " {}={}", bucket_label(i), n); } }
        let _ = writeln!(out, "{}", l);
        let mut l = String::from("tx_rate fail:");
        for (i, n) in self.rate_fail.iter().enumerate() { if *n > 0 || i < 12 { let _ = write!(l, " {}={}", bucket_label(i), n); } }
        let _ = writeln!(out, "{}", l);
        let _ = writeln!(out, "tx_coex: bt_kill {} (frames {}) rts_fail {} tpc_reduced {}", self.bt_kill,
                         self.bt_kill_frames, self.rts_fail, self.tpc_reduced);
        let _ = writeln!(out, "tx_mgmt: ok {} fail {}", self.mgmt_ok, self.mgmt_fail);
    }
}

/// A rate_bucket histogram as ` 1M:n 2M:n ...` (non-zero buckets).
pub fn rate_hist_line(h: &[u64]) -> String {
    let mut s = String::new();
    hist(&mut s, h, bucket_label);
    s
}

fn bucket_label(i: usize) -> String {
    match (LEGACY_R500K.get(i), i) {
        (Some(&r), _) => format!("{}M", mbps(r)),
        (None, 12..=27) => format!("MCS{}", i - 12),
        _ => String::from("other"),
    }
}

fn hist(s: &mut String, h: &[u64], label: impl Fn(usize) -> String) {
    let mut any = false;
    for (i, &n) in h.iter().enumerate() {
        if n > 0 { let _ = write!(s, " {}:{}", label(i), n); any = true; }
    }
    if !any { s.push_str(" -"); }
}
