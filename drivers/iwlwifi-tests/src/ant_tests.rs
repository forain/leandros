//! Rate control: antenna comparison while the link is healthy, the beacon
//! chain-energy hint, and the restart from the beacon signal after idle.

use crate::iwl_assoc::*;
use crate::iwl_rs::*;

const G_AP: [u8; 12] = [0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 0x30, 0x48, 0x60, 0x6c];

fn cfg(fixed: Option<u8>) -> RateCfg {
    RateCfg { ap_rates: G_AP.to_vec(), max_r500k: 0, valid_ant: ANT_A | ANT_B, fixed_ant: fixed, band_24: true, rssi: Some(-57), ht: None }
}

fn rsp(status: u16, ff: u8, rate: u32) -> TxResp {
    let mut b = vec![0u8; 44];
    b[0] = 1;
    b[3] = ff;
    b[4..8].copy_from_slice(&rate.to_le_bytes());
    b[36..38].copy_from_slice(&status.to_le_bytes());
    parse_tx_resp(&b).unwrap()
}

/// Run `secs` seconds of 40 frames/s; antenna `ant` delivers first try with
/// probability `p(ant)` percent (else on the second try).
fn run(c: &mut RateCtl, now: &mut u64, secs: u64, p: impl Fn(u8) -> u32) {
    for i in 0..secs * 40 {
        *now += 25;
        let t = c.sent.unwrap();
        let ant = ((t[0] & RATE_MCS_ANT_MSK) >> RATE_MCS_ANT_POS) as u8;
        let ok = (i * 37 % 100) < p(ant) as u64;
        c.on_tx(&rsp(0x0001, if ok { 0 } else { 1 }, t[0]), *now);
        if c.due(*now) { c.mark_sent(*now); }
    }
}

#[test]
fn equal_antennas_probe_and_return() {
    let mut c = RateCtl::new(&cfg(None));
    let mut now = 1000;
    c.mark_sent(now);
    run(&mut c, &mut now, 100, |_| 97);
    assert!(c.probes >= 2, "{}", c.summary(now));
    assert_eq!(c.probes_kept, 0, "{}", c.summary(now));
    assert_eq!(c.ant, ANT_A);
    assert_eq!(c.toggles, 0);
}

#[test]
fn better_antenna_is_kept() {
    // Antenna A delivers half its frames first try, B nearly all.
    let mut c = RateCtl::new(&cfg(None));
    let mut now = 1000;
    c.mark_sent(now);
    run(&mut c, &mut now, 100, |a| if a == ANT_B { 98 } else { 50 });
    assert_eq!(c.ant, ANT_B, "{}", c.summary(now));
    assert!(c.probes_kept >= 1);
    // And it stays on B: later probes of A go back.
    let kept = c.probes_kept;
    run(&mut c, &mut now, 100, |a| if a == ANT_B { 98 } else { 50 });
    assert_eq!(c.ant, ANT_B, "{}", c.summary(now));
    assert_eq!(c.probes_kept, kept);
}

#[test]
fn fixed_antenna_never_probes() {
    let mut c = RateCtl::new(&cfg(Some(ANT_B)));
    let mut now = 1000;
    c.mark_sent(now);
    run(&mut c, &mut now, 100, |_| 97);
    assert_eq!(c.probes, 0);
    assert_eq!(c.ant, ANT_B);
}

#[test]
fn chain_hint_brings_the_probe_forward() {
    let mut c = RateCtl::new(&cfg(None));
    let mut now = 1000;
    c.mark_sent(now);
    run(&mut c, &mut now, 2, |_| 97);
    let due = c.next_probe_ms;
    assert!(due > now + 20_000);
    c.chain_hint(-70, -66, now);
    assert_eq!(c.next_probe_ms, due, "4 dB is not enough");
    c.chain_hint(-72, -64, now);
    assert!(c.next_probe_ms < due && c.next_probe_ms <= now + ANT_HINT_MIN_MS, "{} vs {}", c.next_probe_ms, now);
    run(&mut c, &mut now, 6, |_| 97);
    assert!(c.probes >= 1);
}

#[test]
fn idle_link_restarts_from_the_beacon_signal() {
    let mut c = RateCtl::new(&cfg(None));
    let mut now = 1000;
    c.mark_sent(now);
    run(&mut c, &mut now, 3, |_| 97);
    assert_eq!(c.cur_r500k(), 108);
    c.on_signal(-83, now);
    assert_eq!(c.cur_r500k(), 108, "windows still fresh: no restart");
    now += STALE_MS + 1000;
    c.on_signal(-83, now);
    assert_eq!(c.cur_r500k(), 36, "-83 dBm on 2.4 GHz starts at 18 M");
    assert_eq!(c.reseeds, 1);
    assert!(c.pending);
}
