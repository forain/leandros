//! SuperSpeed vs USB 2.0 hub protocol (lane usbstick).

use crate::hubproto::*;

/// A 4-port USB 3.0 hub's 0x2A descriptor (VIA VL812 style).
const SS_DESC: [u8; 12] = [12, 0x2A, 4, 0x09, 0x00, 50, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
/// A 7-port USB 2.0 multi-TT hub's 0x29 descriptor.
const HS_DESC: [u8; 9] = [9, 0x29, 7, 0xA9, 0x00, 50, 100, 0x00, 0xFF];

#[test]
fn descriptor_request_differs_by_speed() {
    assert_eq!(descriptor_request(false), (0x2900, 9));
    assert_eq!(descriptor_request(true), (0x2A00, 12));
}

#[test]
fn superspeed_descriptor_parses() {
    let d = parse_descriptor(true, &SS_DESC).unwrap();
    assert_eq!((d.nports, d.reported_ports, d.characteristics, d.power_ms), (4, 4, 0x0009, 100));
}

#[test]
fn usb2_descriptor_parses_and_power_wait_scales() {
    let mut h = HS_DESC;
    h[5] = 100; // 200 ms
    let d = parse_descriptor(false, &h).unwrap();
    assert_eq!((d.nports, d.characteristics, d.power_ms), (7, 0x00A9, 200));
}

#[test]
fn descriptor_type_must_match_hub_speed() {
    assert!(parse_descriptor(false, &SS_DESC).is_err());
    assert!(parse_descriptor(true, &HS_DESC).is_err());
}

#[test]
fn short_or_portless_descriptor_is_refused() {
    assert!(parse_descriptor(true, &SS_DESC[..9]).is_err());
    let mut bad = SS_DESC;
    bad[0] = 9; // bLength says USB 2.0-sized
    assert!(parse_descriptor(true, &bad).is_err());
    let mut none = SS_DESC;
    none[2] = 0;
    assert!(parse_descriptor(true, &none).is_err());
}

#[test]
fn ports_beyond_route_string_are_clamped() {
    let mut big = HS_DESC;
    big[2] = 20;
    let d = parse_descriptor(false, &big).unwrap();
    assert_eq!((d.nports, d.reported_ports), (15, 20));
}

#[test]
fn child_speed_on_superspeed_hub_ignores_usb2_speed_bits() {
    // SS PORT_POWER is bit 9, which is LOW_SPEED on a USB 2.0 hub; the SS
    // connect-speed field (bits 10..12) overlaps HIGH_SPEED.
    let ss_status = PS_CONNECTION | PS_ENABLE | (1 << 9) | (0 << 10) | (0 << 5);
    assert_eq!(child_speed(true, ss_status), ChildSpeed::Super);
    assert_eq!(child_speed(true, ss_status | (1 << 10)), ChildSpeed::Super);
    assert_eq!(child_speed(false, PS_CONNECTION | (1 << 9)), ChildSpeed::Low);
    assert_eq!(child_speed(false, PS_CONNECTION | (1 << 10)), ChildSpeed::High);
    assert_eq!(child_speed(false, PS_CONNECTION | (1 << 8)), ChildSpeed::Full);
}

#[test]
fn warm_reset_completion_counts_only_on_superspeed() {
    assert!(reset_done(false, PC_RESET));
    assert!(!reset_done(false, PC_BH_RESET));
    assert!(reset_done(true, PC_RESET));
    assert!(reset_done(true, PC_BH_RESET));
    assert!(!reset_done(true, PC_CONNECTION));
}

#[test]
fn route_string_per_tier() {
    assert_eq!(route_depth(0), 0);
    assert_eq!(child_route(0, 0, 3), Some(0x3));
    assert_eq!(route_depth(0x3), 1);
    assert_eq!(child_route(0x3, 1, 2), Some(0x23));
    assert_eq!(child_route(0x23, 2, 15), Some(0xF23));
    assert_eq!(route_depth(0x1_2345), 5);
    assert_eq!(child_route(0x1_2345, 5, 1), None);
    assert_eq!(child_route(0, 0, 0), None);
    assert_eq!(child_route(0, 0, 16), None);
}
