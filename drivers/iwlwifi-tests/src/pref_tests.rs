//! Autoconnect ranking: NetworkManager's order plus the 5 GHz preference.

use crate::wifi_pref::*;
use core::sync::atomic::Ordering;

// One test: the knob is a global, and tests run in parallel.
#[test]
fn ranking() {
    five_ghz_preferred_at_boot();
    this_boot_recency_beats_the_band();
    relative_to_the_strongest_24();
    knob_turns_the_preference_off();
}

fn five_ghz_preferred_at_boot() {
    // At boot nothing was connected: the usable 5 GHz BSS beats the legacy
    // 2.4 GHz one with a stronger signal (the laptop's Anvara / Anvara 5G).
    let legacy24 = auto_key(0, 0, 0, true, 6, -73, None);
    let five = auto_key(0, 0, 0, false, 40, -76, None);
    assert!(five > legacy24);
    // Too weak to be usable: the old order (legacy, then signal).
    assert!(auto_key(0, 0, 0, false, 40, -81, None) < legacy24);
    // Priority still comes first.
    assert!(auto_key(1, 0, 0, true, 6, -73, None) > five);
}

fn relative_to_the_strongest_24() {
    // The laptop: 'Anvara 5G' -78 dBm (SNR 14) vs 'Anvara' -73 dBm (SNR 16): 5 GHz.
    assert!(five_usable(40, -78, Some(-73)));
    assert!(auto_key(0, 0, 0, false, 40, -78, Some(-73)) > auto_key(0, 0, 0, true, 6, -73, Some(-73)));
    // A 2.4 GHz BSS 7 dB of SNR ahead wins (wpa_supplicant's SNR rule): -80 vs -64.
    assert!(!five_usable(40, -80, Some(-64)));
    assert!(auto_key(0, 0, 0, false, 40, -80, Some(-64)) < auto_key(0, 0, 0, true, 6, -64, Some(-64)));
    assert!(five_usable(40, -80, Some(-71)), "gap 6 dB");
    // A good 5 GHz SNR (>= 25 dB, -67 dBm) is preferred however strong 2.4 GHz is.
    assert!(five_usable(40, -67, Some(-40)));
    assert!(!five_usable(40, -68, Some(-40)));
    // The absolute floor stays, and 2.4 GHz channels are never "five".
    assert!(!five_usable(40, -81, None));
    assert!(!five_usable(6, -40, None));
    assert_eq!(snr_db(40, -78), 14);
    assert_eq!(snr_db(6, -73), 16);
}

fn this_boot_recency_beats_the_band() {
    // The user chose 2.4 GHz this boot (most recently): autoconnect returns there.
    let b24 = auto_key(0, 5000, 0, true, 6, -73, None);
    let b5 = auto_key(0, 4000, 0, false, 40, -60, None);
    assert!(b24 > b5);
    // A stored timestamp from an earlier boot does not beat the band.
    assert!(auto_key(0, 0, 0, false, 40, -76, None) > auto_key(0, 0, 1_700_000_000, true, 6, -60, None));
    // Same band: the stored timestamp, then legacy, then signal (as before).
    assert!(auto_key(0, 0, 10, false, 6, -80, None) > auto_key(0, 0, 5, true, 6, -40, None));
    assert!(auto_key(0, 0, 0, true, 6, -80, None) > auto_key(0, 0, 0, false, 6, -40, None));
    assert!(auto_key(0, 0, 0, false, 6, -40, None) > auto_key(0, 0, 0, false, 6, -50, None));
}

fn knob_turns_the_preference_off() {
    set_knobs("quiet wifi.prefer5=0 foo=1");
    assert!(!PREFER_5GHZ.load(Ordering::Relaxed));
    assert!(auto_key(0, 0, 0, false, 40, -60, None) < auto_key(0, 0, 0, true, 6, -73, None));
    set_knobs("wifi.prefer5=1");
    assert!(PREFER_5GHZ.load(Ordering::Relaxed));
    set_knobs("nothing");
    assert!(PREFER_5GHZ.load(Ordering::Relaxed));
}
