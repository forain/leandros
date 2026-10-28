//! Wrong-passphrase policy of the autoconnect manager (lane piwifikexec).
//!
//! A "wrong passphrase" verdict from one handshake is weak evidence: at low
//! signal a lost or late EAPOL message 2/3 looks the same. A profile is only
//! blocked for the rest of the boot when the verdict was reached on several
//! different access points within `WK_WINDOW_MS`, and never when the profile
//! is known to work. Pure logic, no kernel dependencies (host-tested).

use alloc::vec::Vec;

/// Verdicts older than this no longer count towards a block.
pub const WK_WINDOW_MS: u64 = 10 * 60 * 1000;
/// Bound on remembered (profile, BSS) verdicts.
pub const WK_MAX: usize = 32;

/// One WRONG_KEY verdict: (profile uuid, BSS, `now_ms`).
pub type WkSeen = ([u8; 16], [u8; 6], u64);

/// Outcome of recording one verdict.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum WkVerdict {
    /// Block the profile; `n` distinct BSSes agreed.
    Block(usize),
    /// Only back off; `n` distinct BSSes so far.
    Defer(usize),
}

/// Record a WRONG_KEY verdict of `uuid` on `bssid` at `now`. `proven`: the
/// profile connected this boot or has a stored timestamp. `threshold` is the
/// number of distinct BSSes needed to block (`wifi.wrongkey_bss`, 1 = block
/// on the first verdict).
pub fn note_wrong_key(seen: &mut Vec<WkSeen>, uuid: [u8; 16], bssid: [u8; 6], now: u64,
                      proven: bool, threshold: u8) -> WkVerdict {
    seen.retain(|s| now.saturating_sub(s.2) <= WK_WINDOW_MS);
    seen.retain(|s| !(s.0 == uuid && s.1 == bssid));
    if seen.len() >= WK_MAX { seen.remove(0); }
    seen.push((uuid, bssid, now));
    let n = seen.iter().filter(|s| s.0 == uuid).count();
    if !proven && n >= threshold.max(1) as usize { WkVerdict::Block(n) } else { WkVerdict::Defer(n) }
}

/// Forget the verdicts of `uuid` (it connected).
pub fn clear_profile(seen: &mut Vec<WkSeen>, uuid: &[u8; 16]) { seen.retain(|s| s.0 != *uuid); }
