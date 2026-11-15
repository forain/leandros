//! Pure Wi-Fi helpers shared by the connection manager (`wifi_mgr`, kernel) and the iwlwifi module:
//! RSN suite constants, handshake failure classification, the PMK derivation, the background-scan
//! gate and the group-key counters (`gk`, shown in /proc/net/wifi). Nothing here touches the kernel
//! except `sched::monotonic_ns` through `kenv`. No credential is ever logged.
#![allow(dead_code)]

use netctl_abi::reason;
use crate::kenv::sched;
use crate::wifi_radio::Fail;

fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }

// ── RSN suite selectors (OUI 00-0f-ac || type, big-endian) ─────────────────

pub const SUITE_TKIP: u32 = 0x000f_ac02;
pub const SUITE_CCMP: u32 = 0x000f_ac04;
pub const SUITE_WEP40: u32 = 0x000f_ac01;
pub const SUITE_WEP104: u32 = 0x000f_ac05;
pub const AKM_8021X: u32 = 0x000f_ac01;
pub const AKM_PSK: u32 = 0x000f_ac02;
pub const AKM_FT_8021X: u32 = 0x000f_ac03;
pub const AKM_FT_PSK: u32 = 0x000f_ac04;
pub const AKM_8021X_SHA256: u32 = 0x000f_ac05;
pub const AKM_PSK_SHA256: u32 = 0x000f_ac06;
pub const AKM_SAE: u32 = 0x000f_ac08;
pub const AKM_FT_SAE: u32 = 0x000f_ac09;
pub const AKM_SUITE_B_192: u32 = 0x000f_ac0c;
pub const AKM_SUITE_B: u32 = 0x000f_ac0b;
pub const AKM_OWE: u32 = 0x000f_ac12;
pub const AKM_SAE_EXT: u32 = 0x000f_ac18;
pub const AKM_FT_SAE_EXT: u32 = 0x000f_ac19;

/// RSN capabilities bits.
pub const RSN_CAP_MFPR: u16 = 1 << 6;
pub const RSN_CAP_MFPC: u16 = 1 << 7;

/// 802.11 reason codes the failure classification looks at.
pub const IEEE_REASON_UNSPECIFIED: u16 = 1;
pub const IEEE_REASON_PREV_AUTH_INVALID: u16 = 2;
pub const IEEE_REASON_LEAVING: u16 = 3;
pub const IEEE_REASON_MIC_FAILURE: u16 = 14;
pub const IEEE_REASON_4WAY_TIMEOUT: u16 = 15;
pub const IEEE_REASON_8021X_FAILED: u16 = 23;


/// Background (roaming) scan interval while connected (only radios with
/// SCAN_CONNECTED), and only while the signal is below `ROAM_SCAN_DBM`:
/// like Linux (no periodic scan while associated without a roaming
/// trigger; NetworkManager's bgscan "simple" is signal-gated too).
pub const CONNECTED_SCAN_MS: u64 = 120_000;
/// The interval in use (tests shorten it; `wifisim.roamscan=<ms>`).
pub static ROAM_SCAN_MS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(CONNECTED_SCAN_MS);
/// A roaming candidate must be at least this much stronger (dB) than the
/// current BSS, so two similar APs do not trade the station back and forth.
pub const ROAM_MARGIN_DB: i32 = 10;
/// Whether a candidate at `cand_dbm` is worth leaving `cur_dbm` for.
pub fn roam_better(cur_dbm: i32, cand_dbm: i32) -> bool { cur_dbm != 0 && cand_dbm >= cur_dbm + ROAM_MARGIN_DB }
/// A connected background scan needs the signal below this (dBm).
pub const ROAM_SCAN_DBM: i32 = -75;

/// Whether the connected loop should start a background scan `since_ms`
/// after the last one, with the current signal `signal_dbm` (0 = unknown).
pub fn connected_bg_scan_due(since_ms: u64, signal_dbm: i32) -> bool {
    since_ms >= ROAM_SCAN_MS.load(core::sync::atomic::Ordering::Relaxed) && signal_dbm != 0 && signal_dbm < ROAM_SCAN_DBM
}

/// Group-key state and TKIP receive counters, shared by the Intel driver and
/// the simulated radio; shown by /proc/net/wifi (`# Group key`).
pub mod gk {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static TKIP: AtomicU64 = AtomicU64::new(0);
    static KEY_ID: AtomicU64 = AtomicU64::new(0);
    static INSTALLS: AtomicU64 = AtomicU64::new(0);
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    /// 0 fw-decrypted, 1 host-decrypted, 2 Michael failures, 3 replay drops, 4 undecryptable.
    static RX: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
    pub const FW: usize = 0;
    pub const SW: usize = 1;
    pub const MIC: usize = 2;
    pub const REPLAY: usize = 3;
    pub const UNDECRYPTED: usize = 4;
    pub fn note_install(key_id: u8, tkip: bool) {
        TKIP.store(tkip as u64 + 1, Relaxed);
        KEY_ID.store(key_id as u64, Relaxed);
        LAST_MS.store(super::now_ms().max(1), Relaxed);
        INSTALLS.fetch_add(1, Relaxed);
    }
    pub fn note_rx(kind: usize) { RX[kind].fetch_add(1, Relaxed); }

    // EAPOL frames (the group-key handshake after association) and frames a
    // driver wait did not claim (scan, session protection, auth/assoc wait).
    static EAPOL_RX: AtomicU64 = AtomicU64::new(0);
    static EAPOL_HELD: AtomicU64 = AtomicU64::new(0);
    static EAPOL_DROPPED: AtomicU64 = AtomicU64::new(0);
    static EAPOL_QFULL: AtomicU64 = AtomicU64::new(0);
    static EAPOL_TID: AtomicU64 = AtomicU64::new(0xff);
    static HELD_DROPPED: AtomicU64 = AtomicU64::new(0);
    /// RX Block Ack reorder drops of frames older than the window, per TID.
    static BA_OLD: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    /// One EAPOL frame received (`tid` 0xff: non-QoS).
    pub fn note_eapol_rx(tid: u8) { EAPOL_RX.fetch_add(1, Relaxed); EAPOL_TID.store(tid as u64, Relaxed); }
    /// One EAPOL frame lost because the driver's notification queue was full.
    pub fn note_eapol_qfull() { EAPOL_QFULL.fetch_add(1, Relaxed); }
    pub fn note_ba_old(tid: u8, n: u64) { if n != 0 { BA_OLD[(tid & 7) as usize].fetch_add(n, Relaxed); } }
    pub fn eapol_counts() -> (u64, u64, u64) { (EAPOL_RX.load(Relaxed), EAPOL_HELD.load(Relaxed), EAPOL_DROPPED.load(Relaxed)) }

    /// Received frames a wait for something else popped (a scan waits up to
    /// 20 s): held here for the connected loop instead of being discarded.
    pub struct Held<T> { q: spin::Mutex<alloc::collections::VecDeque<T>> }
    pub const HELD_CAP: usize = 64;
    impl<T> Held<T> {
        pub const fn new() -> Self { Held { q: spin::Mutex::new(alloc::collections::VecDeque::new()) } }
        pub fn pop(&self) -> Option<T> { self.q.lock().pop_front() }
        pub fn is_empty(&self) -> bool { self.q.lock().is_empty() }
        pub fn clear(&self) { self.q.lock().clear(); }
        /// A frame no wait claimed: `eapol` for an EAPOL frame, `associated`
        /// while a link is up (the connected loop will drain the queue).
        pub fn unclaimed(&self, p: T, eapol: bool, associated: bool) {
            // EAPOL is always kept (group message 1 of a rekey, or the
            // 4-way's message 1 behind the association response); other
            // frames only while associated (deauth, Block Ack requests).
            if !eapol && !associated { return; }
            let mut q = self.q.lock();
            if q.len() >= HELD_CAP {
                HELD_DROPPED.fetch_add(1, Relaxed);
                if eapol { EAPOL_DROPPED.fetch_add(1, Relaxed); }
                return;
            }
            q.push_back(p);
            if eapol { EAPOL_HELD.fetch_add(1, Relaxed); }
        }
    }
    /// A protected data frame the firmware did not decrypt: [unicast,
    /// multicast] x CCMP key index from its header, and the status' cipher
    /// field (0 none, 2 CCM with a MIC failure, 7 error, other).
    static UNDEC: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    static UNDEC_SEC: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    static UNDEC_LAST_MS: AtomicU64 = AtomicU64::new(0);
    static UNDEC_FOREIGN: AtomicU64 = AtomicU64::new(0);
    /// `foreign`: the frame is not from our AP to us / the group (a frame
    /// the address check would have refused anyway).
    pub fn note_undecrypted(frame: &[u8], fc: u16, status: u32, foreign: bool) {
        if foreign { UNDEC_FOREIGN.fetch_add(1, Relaxed); return; }
        let mc = frame.get(4).map_or(false, |b| b & 1 != 0) as usize;
        let ki = frame.get(crate::wifi_frame::hdr_len(fc) + 3).map_or(0, |b| b >> 6) as usize;
        UNDEC[mc * 4 + ki].fetch_add(1, Relaxed);
        let sec = match (status >> 8) & 7 { 0 => 0, 2 => 1, 7 => 2, _ => 3 };
        UNDEC_SEC[sec].fetch_add(1, Relaxed);
        UNDEC_LAST_MS.store(super::now_ms().max(1), Relaxed);
    }
    pub fn undecrypted(mcast: bool, key_idx: usize) -> u64 { UNDEC[mcast as usize * 4 + (key_idx & 3)].load(Relaxed) }
    pub fn installs() -> u64 { INSTALLS.load(Relaxed) }
    pub fn key_id() -> u64 { KEY_ID.load(Relaxed) }
    pub fn rx(kind: usize) -> u64 { RX[kind].load(Relaxed) }
    pub fn lines(out: &mut alloc::string::String) {
        use core::fmt::Write;
        let _ = writeln!(out);
        let _ = writeln!(out, "# Group key (since boot)");
        let c = match TKIP.load(Relaxed) { 1 => "ccmp", 2 => "tkip", _ => "none" };
        let last = LAST_MS.load(Relaxed);
        let _ = writeln!(out, "gtk: cipher {} key_index {} installs {} last_install {}", c, KEY_ID.load(Relaxed), INSTALLS.load(Relaxed),
                         if last == 0 { alloc::string::String::from("never") } else { alloc::format!("{} s ago", super::now_ms().saturating_sub(last) / 1000) });
        let _ = writeln!(out, "gtk_rx: tkip_fw {} tkip_sw {} michael_fail {} replay_drop {} undecryptable {}",
                         rx(FW), rx(SW), rx(MIC), rx(REPLAY), rx(UNDECRYPTED));
        let u = |i: usize| UNDEC[i].load(Relaxed);
        let tid = EAPOL_TID.load(Relaxed);
        let o = |i: usize| BA_OLD[i].load(Relaxed);
        let _ = writeln!(out, "eapol: rx {} held {} dropped {} qfull {} held_overflow {} last_tid {}; ba_old t0 {} t1 {} t2 {} t3 {} t4 {} t5 {} t6 {} t7 {}",
                         EAPOL_RX.load(Relaxed), EAPOL_HELD.load(Relaxed), EAPOL_DROPPED.load(Relaxed), EAPOL_QFULL.load(Relaxed),
                         HELD_DROPPED.load(Relaxed), if tid == 0xff { alloc::string::String::from("none") } else { alloc::format!("{}", tid) },
                         o(0), o(1), o(2), o(3), o(4), o(5), o(6), o(7));
        let ul = UNDEC_LAST_MS.load(Relaxed);
        let _ = writeln!(out, "rx_undecrypted: uc k0 {} k1 {} k2 {} k3 {} mc k0 {} k1 {} k2 {} k3 {} sec none {} mic {} err {} other {} foreign {} last {}",
                         u(0), u(1), u(2), u(3), u(4), u(5), u(6), u(7),
                         UNDEC_SEC[0].load(Relaxed), UNDEC_SEC[1].load(Relaxed), UNDEC_SEC[2].load(Relaxed), UNDEC_SEC[3].load(Relaxed), UNDEC_FOREIGN.load(Relaxed),
                         if ul == 0 { alloc::string::String::from("never") } else { alloc::format!("{} ms ago", super::now_ms().saturating_sub(ul)) });
    }
}

/// The 4-way handshake ended without keys: decide between a wrong secret
/// and a plain timeout. The pattern of a wrong passphrase on WPA2-PSK is
/// that the AP sent message 1, got our message 2, could not verify its MIC
/// (the PMK differs) and so never sends message 3: it retransmits message 1
/// and finally deauthenticates with reason 15 (4-way handshake timeout),
/// some APs with 14 (MIC failure) or 23 (802.1X failed), others just go
/// quiet. Without a message 1 at all it is a timeout, not the key.
pub fn handshake_failure(msg2_sent: bool, deauth_reason: Option<u16>) -> Fail {
    handshake_failure_ev(msg2_sent, None, deauth_reason)
}

/// `handshake_failure` with EAPOL evidence from a host supplicant:
/// `mic_suspect` = Some(true) when the AP re-sent message 1 after our
/// message 2 (it could not verify the MIC), Some(false) when it did not.
/// Reasons 1 (unspecified) and 2 (previous authentication no longer valid)
/// are not specific to the key: without that evidence they mean the AP
/// rejected us, a fault of that BSS (backed off, autoconnect moves on),
/// not a wrong passphrase (which blocks the profile).
pub fn handshake_failure_ev(msg2_sent: bool, mic_suspect: Option<bool>, deauth_reason: Option<u16>) -> Fail {
    match deauth_reason {
        Some(r) if msg2_sent && mic_suspect == Some(false) && matches!(r, IEEE_REASON_PREV_AUTH_INVALID | IEEE_REASON_UNSPECIFIED) =>
            Fail::new(reason::DEAUTH_BY_AP, alloc::format!("AP rejected (reason {}) after message 2; the AP did not re-send message 1, so probably not a wrong passphrase", r)).ieee_reason(r),
        Some(r) if msg2_sent && matches!(r, IEEE_REASON_4WAY_TIMEOUT | IEEE_REASON_MIC_FAILURE | IEEE_REASON_8021X_FAILED
            | IEEE_REASON_PREV_AUTH_INVALID | IEEE_REASON_UNSPECIFIED) =>
            Fail::new(reason::WRONG_KEY, alloc::format!("the AP ended the handshake (reason {}): wrong passphrase", r)).ieee_reason(r),
        Some(r) => Fail::new(reason::DEAUTH_BY_AP, alloc::format!("deauthenticated during the handshake (reason {})", r)).ieee_reason(r),
        None if msg2_sent => Fail::new(reason::WRONG_KEY, "4-way handshake timed out after message 2: wrong passphrase"),
        None => Fail::new(reason::TIMEOUT, "no EAPOL message 1 from the AP"),
    }
}

/// The PMK for WPA2-PSK: 64 hex digits are the PMK itself, anything else is
/// a passphrase (PBKDF2-SHA1, 4096 rounds over the SSID).
pub fn pmk_for(ssid: &[u8], psk: &[u8]) -> [u8; 32] {
    if psk.len() == 64 && psk.iter().all(|c| c.is_ascii_hexdigit()) {
        let mut pmk = [0u8; 32];
        let h = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
        for i in 0..32 { pmk[i] = (h(psk[2 * i]) << 4) | h(psk[2 * i + 1]); }
        return pmk;
    }
    crate::wifi_crypto::wpa_psk(psk, ssid)
}
