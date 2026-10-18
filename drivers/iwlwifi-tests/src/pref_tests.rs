//! Autoconnect ranking: NetworkManager's order plus the 5 GHz preference.

use crate::wifi_pref::*;
use core::sync::atomic::Ordering;

// One test: the knob is a global, and tests run in parallel.
#[test]
fn ranking() {
    five_ghz_preferred_at_boot();
    this_boot_recency_beats_the_band();
    knob_turns_the_preference_off();
}

fn five_ghz_preferred_at_boot() {
    // At boot nothing was connected: the usable 5 GHz BSS beats the legacy
    // 2.4 GHz one with a stronger signal (the laptop's Anvara / Anvara 5G).
    let legacy24 = auto_key(0, 0, 0, true, 6, -73);
    let five = auto_key(0, 0, 0, false, 40, -76);
    assert!(five > legacy24);
    // Too weak to be usable: the old order (legacy, then signal).
    assert!(auto_key(0, 0, 0, false, 40, -81) < legacy24);
    // Priority still comes first.
    assert!(auto_key(1, 0, 0, true, 6, -73) > five);
}

fn this_boot_recency_beats_the_band() {
    // The user chose 2.4 GHz this boot (most recently): autoconnect returns there.
    let b24 = auto_key(0, 5000, 0, true, 6, -73);
    let b5 = auto_key(0, 4000, 0, false, 40, -60);
    assert!(b24 > b5);
    // A stored timestamp from an earlier boot does not beat the band.
    assert!(auto_key(0, 0, 0, false, 40, -76) > auto_key(0, 0, 1_700_000_000, true, 6, -60));
    // Same band: the stored timestamp, then legacy, then signal (as before).
    assert!(auto_key(0, 0, 10, false, 6, -80) > auto_key(0, 0, 5, true, 6, -40));
    assert!(auto_key(0, 0, 0, true, 6, -80) > auto_key(0, 0, 0, false, 6, -40));
    assert!(auto_key(0, 0, 0, false, 6, -40) > auto_key(0, 0, 0, false, 6, -50));
}

fn knob_turns_the_preference_off() {
    set_knobs("quiet wifi.prefer5=0 foo=1");
    assert!(!PREFER_5GHZ.load(Ordering::Relaxed));
    assert!(auto_key(0, 0, 0, false, 40, -60) < auto_key(0, 0, 0, true, 6, -73));
    set_knobs("wifi.prefer5=1");
    assert!(PREFER_5GHZ.load(Ordering::Relaxed));
    set_knobs("nothing");
    assert!(PREFER_5GHZ.load(Ordering::Relaxed));
}
