//! Autoconnect's ranking of candidate networks (wifi_mgr), hardware
//! independent and host tested (drivers/iwlwifi-tests).

/// A 5 GHz BSS at least this strong is preferred to 2.4 GHz ones by
/// autoconnect (`wifi.prefer5=0` turns the preference off), unless a 2.4 GHz
/// candidate is clearly stronger (`PREFER_5GHZ_MAX_SNR_GAP`).
pub const PREFER_5GHZ_MIN_DBM: i16 = -80;
/// wpa_supplicant's default noise floors (DEFAULT_NOISE_FLOOR_2GHZ / _5GHZ):
/// signal - floor = the SNR its BSS ranking uses.
pub const NOISE_FLOOR_24_DBM: i16 = -89;
pub const NOISE_FLOOR_5_DBM: i16 = -92;
/// wpa_scan_result_compar: SNRs closer than 7 dB are compared by estimated
/// throughput (where 5 GHz, wider and less crowded, wins); a bigger gap is
/// decided by the SNR. Above GOOD_SNR (25 dB) the 5 GHz BSS is good enough
/// whatever the 2.4 GHz one has.
pub const PREFER_5GHZ_MAX_SNR_GAP: i16 = 7;
pub const PREFER_5GHZ_GOOD_SNR: i16 = 25;
/// `wifi.prefer5` (default on): see `auto_key`.
pub static PREFER_5GHZ: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// `wifi.wrongkey_bss=N` (default 2): distinct access points that must agree on
/// "wrong passphrase" before the profile is blocked for the boot (1 = on the
/// first verdict, the old behaviour).
pub static WRONGKEY_BSS: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(2);
pub fn wrongkey_bss() -> u8 { WRONGKEY_BSS.load(core::sync::atomic::Ordering::Relaxed) }

/// Read the manager's kernel command-line knobs (`wifi.prefer5=0|1`, `wifi.wrongkey_bss=N`).
pub fn set_knobs(cmdline: &str) {
    if let Some(n) = cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix("wifi.wrongkey_bss="))
        .filter_map(|v| v.parse::<u8>().ok()).last() {
        WRONGKEY_BSS.store(n.max(1), core::sync::atomic::Ordering::Relaxed);
    }
    if let Some(v) = cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix("wifi.prefer5=")).last() {
        PREFER_5GHZ.store(v != "0", core::sync::atomic::Ordering::Relaxed);
    }
}

/// The SNR (dB) wpa_supplicant would assume for a BSS on `channel`.
pub fn snr_db(channel: u8, signal_dbm: i16) -> i16 {
    signal_dbm - if channel > 14 { NOISE_FLOOR_5_DBM } else { NOISE_FLOOR_24_DBM }
}

/// The 5 GHz BSS (`channel`, `signal_dbm`) is worth preferring over the
/// 2.4 GHz candidates, the strongest of which is `best_24_dbm` (None: no
/// 2.4 GHz candidate).
pub fn five_usable(channel: u8, signal_dbm: i16, best_24_dbm: Option<i16>) -> bool {
    if channel <= 14 || signal_dbm < PREFER_5GHZ_MIN_DBM { return false; }
    let snr5 = snr_db(channel, signal_dbm);
    match best_24_dbm {
        None => true,
        Some(s24) => snr5 >= PREFER_5GHZ_GOOD_SNR || snr_db(1, s24) - snr5 < PREFER_5GHZ_MAX_SNR_GAP,
    }
}

/// The autoconnect ranking of one (profile, BSS) candidate; larger wins.
/// NetworkManager's order (priority, recency, legacy, signal) with one
/// addition: a usable 5 GHz BSS ([`five_usable`]: channel > 14, at least
/// `PREFER_5GHZ_MIN_DBM`, and not 7 dB of SNR behind the strongest 2.4 GHz
/// candidate `best_24_dbm`) beats 2.4 GHz ones of the same priority unless
/// a network was connected this boot (`boot_ms`: when; 0 = not this boot,
/// the most recent wins). `stored_s` is the profile's stored timestamp. On the laptop
/// 'Anvara 5G' at -75 dBm gave 5x the throughput of 'Anvara' (2.4 GHz) at
/// -73 dBm (lane wifi2), as the 2.4 GHz channels are shared by many BSSs.
pub fn auto_key(priority: i32, boot_ms: u64, stored_s: u64, legacy: bool, channel: u8, signal_dbm: i16,
                best_24_dbm: Option<i16>) -> (i64, u64, bool, u64, bool, i16) {
    let five = PREFER_5GHZ.load(core::sync::atomic::Ordering::Relaxed) && five_usable(channel, signal_dbm, best_24_dbm);
    (priority as i64, boot_ms, five, stored_s, legacy, signal_dbm)
}
