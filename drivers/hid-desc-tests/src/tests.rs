use crate::hid_desc::*;

/// A Windows-Precision-Touchpad style descriptor: 5 fingers, X 0..1200,
/// Y 0..800, cm units exp -2, report id 1 input, id 2 max contacts feature,
/// id 3 Input Mode in the Device Configuration collection.
fn ptp_desc() -> Vec<u8> {
    let mut d: Vec<u8> = vec![0x05, 0x0D, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01];
    for _ in 0..5 {
        d.extend_from_slice(&[
            0x05, 0x0D, 0x09, 0x22, 0xA1, 0x02,
            0x09, 0x47, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02, 0x81, 0x02,
            0x95, 0x01, 0x75, 0x02, 0x81, 0x03,
            0x09, 0x51, 0x75, 0x04, 0x95, 0x01, 0x25, 0x0F, 0x81, 0x02,
            0x05, 0x01, 0x09, 0x30, 0x26, 0xB0, 0x04, 0x75, 0x10, 0x55, 0x0E, 0x65, 0x11,
            0x35, 0x00, 0x46, 0x78, 0x00, 0x95, 0x01, 0x81, 0x02,
            0x09, 0x31, 0x26, 0x20, 0x03, 0x46, 0x50, 0x00, 0x81, 0x02,
            0xC0,
        ]);
    }
    d.extend_from_slice(&[
        0x05, 0x0D, 0x27, 0xFF, 0xFF, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0x09, 0x56, 0x81, 0x02,
        0x09, 0x54, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
        0x05, 0x09, 0x09, 0x01, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02,
        0x95, 0x07, 0x81, 0x03,
        0x05, 0x0D, 0x85, 0x02, 0x09, 0x55, 0x25, 0x05, 0x75, 0x08, 0x95, 0x01, 0xB1, 0x02,
        0xC0,
        0x05, 0x0D, 0x09, 0x0E, 0xA1, 0x01, 0x85, 0x03, 0x09, 0x22, 0xA1, 0x02,
        0x09, 0x52, 0x25, 0x0A, 0x75, 0x08, 0x95, 0x01, 0xB1, 0x02, 0xC0, 0xC0,
    ]);
    d
}

#[test]
fn ptp_five_fingers_and_ranges() {
    let d = parse(&ptp_desc());
    assert_eq!(d.apps, vec![APP_TOUCHPAD, APP_CONFIG]);
    let p = ptp(&d).unwrap();
    assert_eq!(p.fingers.len(), 5);
    assert_eq!(p.input_id, 1);
    let x = p.x.unwrap();
    assert_eq!((x.lmin, x.lmax, x.pmax, x.unit_exp), (0, 1200, 120, -2));
    let y = p.y.unwrap();
    assert_eq!((y.lmin, y.lmax, y.pmax), (0, 800, 80));
    for f in &p.fingers {
        assert!(f.tip.is_some() && f.conf.is_some() && f.cid.is_some() && f.x.is_some() && f.y.is_some());
    }
    // 40 bits per finger, so finger 1's X starts at bit 40 + 8
    assert_eq!(p.fingers[1].x.unwrap().loc.bit, 48);
}

#[test]
fn ptp_mode_and_contact_features() {
    let p = ptp(&parse(&ptp_desc())).unwrap();
    assert_eq!(p.mode_feature.unwrap().id, 3);
    assert_eq!(p.max_contacts_feature.unwrap().id, 2);
    assert!(p.contact_count.is_some() && p.button1.is_some() && p.scan_time.is_some());
}

#[test]
fn summary_mentions_touchpad() {
    let s = summary(&parse(&ptp_desc()));
    assert!(s.contains("0d:05 touchpad"), "{}", s);
    assert!(s.contains("fingers=5"), "{}", s);
}

#[test]
fn mouse_has_no_ptp() {
    let d = parse(&[0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
        0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0xC0, 0xC0]);
    assert_eq!(d.apps, vec![APP_MOUSE]);
    assert!(ptp(&d).is_none());
    assert_eq!(d.fields[0].usages.len(), 3);
}

// ---- lapptp: PTP report decoding ----

use crate::hid_ptp::*;

/// ptp_desc() plus a mouse collection (report id 4: 3 buttons, 5 pad bits, rel X/Y/wheel).
fn full_desc() -> Vec<u8> {
    let mut d = ptp_desc();
    d.extend_from_slice(&[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x04, 0x09, 0x01, 0xA1, 0x00,
        0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x05, 0x81, 0x03,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06,
        0xC0, 0xC0,
    ]);
    d
}

fn put(p: &mut [u8], bit: usize, size: usize, v: u32) {
    for i in 0..size { if (v >> i) & 1 != 0 { p[(bit + i) / 8] |= 1 << ((bit + i) % 8); } }
}

/// Report (with id byte) from (cid, tip, conf, x, y) tuples.
fn report(fingers: &[(u32, u32, u32, u32, u32)], count: u32, btn: u32) -> Vec<u8> {
    let mut r = vec![0u8; 30];
    r[0] = 1;
    let p = &mut r[1..];
    for (i, f) in fingers.iter().enumerate() {
        let b = i * 40;
        put(p, b, 1, f.2); // confidence
        put(p, b + 1, 1, f.1); // tip
        put(p, b + 4, 4, f.0);
        put(p, b + 8, 16, f.3);
        put(p, b + 24, 16, f.4);
    }
    put(p, 200, 16, 1234);
    put(p, 216, 8, count);
    put(p, 224, 1, btn);
    r
}

#[test]
fn decode_two_fingers_and_button() {
    let d = parse(&full_desc());
    let p = ptp(&d).unwrap();
    let r = report(&[(0, 1, 1, 600, 300), (1, 1, 1, 1273, 672)], 2, 1);
    let (id, f) = parse_report(&p, d.uses_report_ids, &r).unwrap();
    assert_eq!(id, 1);
    assert_eq!(f.count, 2);
    assert!(f.button);
    assert_eq!((f.contacts[0].x, f.contacts[0].y, f.contacts[0].cid), (600, 300, 0));
    assert_eq!((f.contacts[1].x, f.contacts[1].y, f.contacts[1].cid), (1273, 672, 1));
    assert!(f.contacts[2..].iter().all(|c| !c.tip));
    let mut other = r.clone();
    other[0] = 9;
    assert!(parse_report(&p, true, &other).is_none());
}

#[test]
fn mt_protocol_b_sequence() {
    let d = parse(&full_desc());
    let p = ptp(&d).unwrap();
    let mut mt = Mt::default();
    let mut ev = Vec::new();
    let down = parse_report(&p, true, &report(&[(0, 1, 1, 100, 200)], 1, 0)).unwrap().1;
    mt.frame(&down, &mut ev);
    assert_eq!(&ev[..4], &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 0), (EV_ABS, ABS_MT_POSITION_X, 100), (EV_ABS, ABS_MT_POSITION_Y, 200)]);
    assert!(ev.contains(&(EV_KEY, BTN_TOUCH, 1)) && ev.contains(&(EV_KEY, 0x145, 1)) && ev.contains(&(EV_ABS, ABS_X, 100)));
    assert_eq!(*ev.last().unwrap(), (EV_SYN, 0, 0));
    // second finger: tool becomes DOUBLETAP; first finger unchanged emits nothing
    ev.clear();
    let two = parse_report(&p, true, &report(&[(0, 1, 1, 100, 200), (1, 1, 1, 500, 90)], 2, 0)).unwrap().1;
    mt.frame(&two, &mut ev);
    assert_eq!(ev[0], (EV_ABS, ABS_MT_SLOT, 1));
    assert!(ev.contains(&(EV_ABS, ABS_MT_TRACKING_ID, 1)));
    assert!(ev.contains(&(EV_KEY, 0x145, 0)) && ev.contains(&(EV_KEY, 0x14d, 1)));
    // move finger 0 in x only
    ev.clear();
    let mv = parse_report(&p, true, &report(&[(0, 1, 1, 110, 200), (1, 1, 1, 500, 90)], 2, 0)).unwrap().1;
    mt.frame(&mv, &mut ev);
    assert_eq!(&ev[..2], &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_POSITION_X, 110)]);
    assert!(!ev.iter().any(|e| e.1 == ABS_MT_POSITION_Y));
    // lift both
    ev.clear();
    let up = parse_report(&p, true, &report(&[(0, 0, 1, 110, 200), (1, 0, 1, 500, 90)], 0, 0)).unwrap().1;
    mt.frame(&up, &mut ev);
    assert_eq!(ev.iter().filter(|e| **e == (EV_ABS, ABS_MT_TRACKING_ID, -1)).count(), 2);
    assert!(ev.contains(&(EV_KEY, BTN_TOUCH, 0)) && ev.contains(&(EV_KEY, 0x14d, 0)));
    assert_eq!(mt.active(), 0);
}

#[test]
fn palm_is_dropped() {
    let p = ptp(&parse(&full_desc())).unwrap();
    let mut mt = Mt::default();
    let mut ev = Vec::new();
    mt.frame(&parse_report(&p, true, &report(&[(0, 1, 0, 100, 200)], 1, 0)).unwrap().1, &mut ev);
    assert_eq!(mt.active(), 0);
    assert!(!ev.iter().any(|e| e.1 == ABS_MT_TRACKING_ID));
}

#[test]
fn button_click_without_touch() {
    let p = ptp(&parse(&full_desc())).unwrap();
    let mut mt = Mt::default();
    let mut ev = Vec::new();
    mt.frame(&parse_report(&p, true, &report(&[], 0, 1)).unwrap().1, &mut ev);
    assert!(ev.contains(&(EV_KEY, BTN_LEFT, 1)));
}

#[test]
fn hybrid_frame_spans_reports() {
    let p = ptp(&parse(&full_desc())).unwrap();
    let mut a = Assembler::default();
    let mk = |n: usize, cnt| {
        let fs: Vec<_> = (0..n).map(|i| (i as u32, 1, 1, 10 * i as u32, 5)).collect();
        parse_report(&p, true, &report(&fs, cnt, 0)).unwrap().1
    };
    // 7 contacts over two 5-finger reports
    assert!(a.push(mk(5, 7), 5).is_none());
    let f = a.push(mk(2, 0), 5).unwrap();
    assert_eq!(f.contacts.len(), 7);
    // a single complete report passes straight through
    assert_eq!(a.push(mk(2, 2), 5).unwrap().contacts.len(), 2);
}

#[test]
fn mouse_collection_decodes() {
    let d = parse(&full_desc());
    let m = mouse_layout(&d).unwrap();
    assert_eq!(m.id, 4);
    assert_eq!(parse_mouse(&m, true, &[4, 0b101, 0xFE, 5, 0]), Some((-2, 5, 0, 0b101)));
    assert_eq!(parse_mouse(&m, true, &[1, 0, 0, 0, 0]), None);
}

#[test]
fn mode_feature_payload() {
    let p = ptp(&parse(&full_desc())).unwrap();
    let m = p.mode_feature.unwrap();
    assert_eq!(m.id, 3);
    assert_eq!(feature_payload(m, 3), vec![3]);
}

#[test]
fn config_report_sets_mode_and_switches() {
    // id 3 report: Input Mode (8 bits), Surface Switch (1), Button Switch (1), 6 pad bits
    let mut d = ptp_desc();
    d.truncate(d.len() - 12);
    d.extend_from_slice(&[0x09, 0x52, 0x25, 0x0A, 0x75, 0x08, 0x95, 0x01, 0xB1, 0x02,
        0x09, 0x57, 0x09, 0x58, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02, 0xB1, 0x02, 0x95, 0x06, 0xB1, 0x03, 0xC0, 0xC0]);
    let p = parse(&d);
    let r = config_report(&p, 3);
    assert_eq!(r, vec![3, 0b11]);
}
