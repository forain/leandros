//! Autoconnect's ranking of candidate networks (wifi_mgr), hardware
//! independent and host tested (drivers/iwlwifi-tests).

/// A 5 GHz BSS at least this strong is preferred to 2.4 GHz ones by
/// autoconnect (`wifi.prefer5=0` turns the preference off).
pub const PREFER_5GHZ_MIN_DBM: i16 = -80;
/// `wifi.prefer5` (default on): see `auto_key`.
pub static PREFER_5GHZ: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Read the manager's kernel command-line knobs (`wifi.prefer5=0|1`).
pub fn set_knobs(cmdline: &str) {
    if let Some(v) = cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix("wifi.prefer5=")).last() {
        PREFER_5GHZ.store(v != "0", core::sync::atomic::Ordering::Relaxed);
    }
}

/// The autoconnect ranking of one (profile, BSS) candidate; larger wins.
/// NetworkManager's order (priority, recency, legacy, signal) with one
/// addition: a usable 5 GHz BSS (channel > 14, at least
/// `PREFER_5GHZ_MIN_DBM`) beats 2.4 GHz ones of the same priority unless
/// a network was connected this boot (`boot_ms`: when; 0 = not this boot,
/// the most recent wins). `stored_s` is the profile's stored timestamp. On the laptop
/// 'Anvara 5G' at -75 dBm gave 5x the throughput of 'Anvara' (2.4 GHz) at
/// -73 dBm (lane wifi2), as the 2.4 GHz channels are shared by many BSSs.
pub fn auto_key(priority: i32, boot_ms: u64, stored_s: u64, legacy: bool, channel: u8, signal_dbm: i16)
                -> (i64, u64, bool, u64, bool, i16) {
    let five = PREFER_5GHZ.load(core::sync::atomic::Ordering::Relaxed) && channel > 14 && signal_dbm >= PREFER_5GHZ_MIN_DBM;
    (priority as i64, boot_ms, five, stored_s, legacy, signal_dbm)
}
