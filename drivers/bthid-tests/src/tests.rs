use crate::report::*;
use alloc::vec;
use alloc::vec::Vec;

fn e(role: Role, ty: u16, code: u16, value: i32) -> Ev { Ev { role, ty, code, value } }
fn ga(code: u16, v: i32) -> Ev { e(Role::Gamepad, EV_ABS, code, v) }
fn gk(code: u16, v: i32) -> Ev { e(Role::Gamepad, EV_KEY, code, v) }
fn syn(role: Role) -> Ev { e(role, EV_SYN, 0, 0) }

// 8BitDo-style D-input gamepad: GD Gamepad, report id 3, X/Y/Z/Rz 8-bit,
// hat (4 bit, null state), 15 buttons, Simulation Brake/Accelerator.
// reconstructed
const DESC_8BITDO: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x03,
    0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02,
    0x09, 0x39, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46, 0x3B, 0x01,
    0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42,
    0x75, 0x04, 0x95, 0x01, 0x81, 0x03,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x0F, 0x81, 0x02,
    0x75, 0x01, 0x95, 0x01, 0x81, 0x03,
    0x05, 0x02, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x02,
    0x09, 0xC5, 0x09, 0xC4, 0x81, 0x02,
    0xC0,
];

fn r8(x: u8, y: u8, z: u8, rz: u8, hat: u8, b: u16, l2: u8, r2: u8) -> [u8; 10] {
    [3, x, y, z, rz, hat, b as u8, (b >> 8) as u8, l2, r2]
}

#[test]
fn eightbitdo_dinput() {
    let mut d = HidDevice::new(0x2dc8, 0x3106, DESC_8BITDO);
    assert!(d.has(Role::Gamepad) && !d.has(Role::Mouse));
    let v = d.input(&r8(0x80, 0x80, 0x80, 0x80, 0x0F, 0, 0, 0));
    assert_eq!(v, vec![ga(ABS_X, 128), ga(ABS_Y, 128), ga(ABS_RX, 128), ga(ABS_RY, 128), syn(Role::Gamepad)]);
    // unchanged -> nothing
    assert!(d.input(&r8(0x80, 0x80, 0x80, 0x80, 0x0F, 0, 0, 0)).is_empty());
    assert_eq!(d.input(&r8(0x80, 0x80, 0x80, 0x80, 0x0F, 1, 0, 0)), vec![gk(BTN_SOUTH, 1), syn(Role::Gamepad)]);
    assert_eq!(d.input(&r8(0x80, 0x80, 0x80, 0x80, 0x0F, 0, 0, 0)), vec![gk(BTN_SOUTH, 0), syn(Role::Gamepad)]);
    assert_eq!(d.input(&r8(0x80, 0x80, 0x80, 0x80, 2, 0, 0, 0)), vec![ga(ABS_HAT0X, 1), syn(Role::Gamepad)]);
    assert_eq!(d.input(&r8(0x80, 0x80, 0x80, 0x80, 0x0F, 0, 0, 0)), vec![ga(ABS_HAT0X, 0), syn(Role::Gamepad)]);
    assert_eq!(d.input(&r8(0, 0x80, 0x80, 0x80, 0x0F, 0, 0, 0)), vec![ga(ABS_X, -32768), syn(Role::Gamepad)]);
    // Z/Rz = right stick when there is no Rx/Ry; Rz=0 is up
    assert_eq!(d.input(&r8(0, 0x80, 0x80, 0, 0x0F, 0, 0, 0)), vec![ga(ABS_RY, -32768), syn(Role::Gamepad)]);
    assert_eq!(d.input(&r8(0, 0x80, 0x80, 0, 0x0F, 0, 127, 127)),
        vec![ga(ABS_Z, 127), ga(ABS_RZ, 127), syn(Role::Gamepad)]);
    // button 15 and release of everything
    let v = d.input(&r8(0x80, 0x80, 0x80, 0x80, 0x0F, 0x4000, 0, 0));
    assert!(v.contains(&gk(BTN_SOUTH + 14, 1)));
    let v = d.input(&r8(0x80, 0x80, 0x80, 0x80, 0x0F, 0, 0, 0));
    assert!(v.contains(&gk(BTN_SOUTH + 14, 0)));
}

#[test]
fn ds4_reports() {
    assert_eq!(quirk_for(0x054c, 0x05c4), Quirk::Ds4);
    // full 0x11
    let mut d = HidDevice::new(0x054c, 0x05c4, &[]);
    let mut r = [0u8; 78];
    r[0] = 0x11;
    for i in 3..7 { r[i] = 0x80; }
    r[7] = 0x08;
    let v = d.input(&r);
    assert_eq!(v, vec![ga(ABS_X, 128), ga(ABS_Y, 128), ga(ABS_RX, 128), ga(ABS_RY, 128), syn(Role::Gamepad)]);
    r[7] = 0x22; // cross + hat east
    r[9] = 1; // PS
    r[10] = 0xFF; // L2
    assert_eq!(d.input(&r), vec![ga(ABS_Z, 255), ga(ABS_HAT0X, 1), gk(BTN_SOUTH, 1), gk(BTN_MODE, 1), syn(Role::Gamepad)]);
    // reduced 0x01
    let mut d = HidDevice::new(0x054c, 0x05c4, &[]);
    let mut r = [0u8; 10];
    r[0] = 1;
    r[1] = 0; r[2] = 255; r[3] = 0x80; r[4] = 0x80;
    r[5] = 0x88; // triangle? 0x80 = triangle, hat 8 = neutral
    r[6] = 0x20; // options
    r[8] = 0x10;
    let v = d.input(&r);
    assert!(v.contains(&ga(ABS_X, -32768)) && v.contains(&ga(ABS_Y, 32767)));
    assert!(v.contains(&gk(BTN_NORTH, 1)) && v.contains(&gk(BTN_START, 1)) && v.contains(&ga(ABS_Z, 16)));
    // short / unknown reports are ignored
    assert!(d.input(&[0x01, 1, 2]).is_empty());
    assert!(d.input(&[]).is_empty());
    assert!(d.input(&[0x77; 40]).is_empty());
}

#[test]
fn dualsense_reports() {
    assert_eq!(quirk_for(0x054c, 0x0ce6), Quirk::DualSense);
    assert_eq!(init_control(Quirk::DualSense), Some([0x43, 0x05]));
    assert_eq!(init_control(Quirk::Ds4), Some([0x43, 0x02]));
    assert_eq!(init_control(Quirk::Generic), None);
    let mut d = HidDevice::new(0x054c, 0x0ce6, &[]);
    let mut r = [0u8; 78];
    r[0] = 0x31; r[1] = 0x10;
    r[2] = 0; r[3] = 255; r[4] = 0x80; r[5] = 0x80;
    r[6] = 10; r[7] = 20;
    r[9] = 0x88; // triangle + neutral hat
    r[10] = 0x20; // options
    let v = d.input(&r);
    assert_eq!(v, vec![ga(ABS_X, -32768), ga(ABS_Y, 32767), ga(ABS_RX, 128), ga(ABS_RY, 128),
        ga(ABS_Z, 10), ga(ABS_RZ, 20), gk(BTN_NORTH, 1), gk(BTN_START, 1), syn(Role::Gamepad)]);
    r[9] = 0x08; r[10] = 0;
    assert_eq!(d.input(&r), vec![gk(BTN_NORTH, 0), gk(BTN_START, 0), syn(Role::Gamepad)]);
}

#[test]
fn switch_pro() {
    assert_eq!(quirk_for(0x057e, 0x2009), Quirk::SwitchPro);
    let mut d = HidDevice::new(0x057e, 0x2009, &[]);
    // simple HID 0x3F
    let rep = |b0: u8, hat: u8, lx: u16| {
        let mut r = vec![0x3F, b0, 0, hat];
        for v in [lx, 0x8000, 0x8000, 0x8000] { r.extend_from_slice(&v.to_le_bytes()); }
        r
    };
    let v = d.input(&rep(0, 8, 0x8000));
    assert_eq!(v, vec![ga(ABS_X, 0), ga(ABS_Y, 0), ga(ABS_RX, 0), ga(ABS_RY, 0), syn(Role::Gamepad)]);
    assert_eq!(d.input(&rep(1, 2, 0xFFFF)),
        vec![gk(BTN_SOUTH, 1), ga(ABS_HAT0X, 1), ga(ABS_X, 32767), syn(Role::Gamepad)]);
    // full 0x30, 12-bit packed sticks
    let mut d = HidDevice::new(0x057e, 0x2009, &[]);
    let mut r = [0u8; 12];
    r[0] = 0x30;
    let pack = |r: &mut [u8; 12], o: usize, x: u16, y: u16| {
        r[o] = x as u8;
        r[o + 1] = ((x >> 8) & 0xF) as u8 | ((y & 0xF) << 4) as u8;
        r[o + 2] = (y >> 4) as u8;
    };
    pack(&mut r, 6, 2048, 2048);
    pack(&mut r, 9, 2048, 2048);
    let v = d.input(&r);
    let near0 = |c: u16| v.iter().any(|x| x.code == c && x.ty == EV_ABS && x.value.abs() < 20);
    assert!(near0(ABS_X) && near0(ABS_Y) && near0(ABS_RX) && near0(ABS_RY));
    r[3] = 0x04; // B -> south
    r[5] = 0x04; // dpad right
    pack(&mut r, 6, 2048, 4095); // stick pushed up (raw Y high)
    pack(&mut r, 9, 0, 2048);
    let v = d.input(&r);
    assert!(v.contains(&gk(BTN_SOUTH, 1)));
    assert!(v.contains(&ga(ABS_HAT0X, 1)));
    assert!(v.contains(&ga(ABS_Y, -32768)));
    assert!(v.contains(&ga(ABS_RX, -32768)));
}

// Boot-style keyboard (id 1) + consumer control (id 2). reconstructed
const DESC_KBD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01,
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
    0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00,
    0xC0,
    0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x02,
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x09, 0xE9, 0x81, 0x02,
    0x75, 0x07, 0x95, 0x01, 0x81, 0x03,
    0xC0,
];

#[test]
fn keyboard_and_consumer() {
    let mut d = HidDevice::new(0x1234, 0x5678, DESC_KBD);
    assert!(d.has(Role::Keyboard) && !d.has(Role::Gamepad));
    let k = Role::Keyboard;
    assert!(d.input(&[1, 0, 0, 0, 0, 0, 0, 0, 0]).is_empty());
    assert_eq!(d.input(&[1, 0, 0, 4, 0, 0, 0, 0, 0]), vec![e(k, EV_KEY, 30, 1), syn(k)]);
    // rollover error ignored: nothing, and A stays down
    assert!(d.input(&[1, 0, 0, 1, 1, 1, 1, 1, 1]).is_empty());
    assert_eq!(d.input(&[1, 0x02, 0, 4, 0, 0, 0, 0, 0]), vec![e(k, EV_KEY, 42, 1), syn(k)]);
    assert_eq!(d.input(&[1, 0x02, 0, 0, 0, 0, 0, 0, 0]), vec![e(k, EV_KEY, 30, 0), syn(k)]);
    assert_eq!(d.input(&[1, 0, 0, 0, 0, 0, 0, 0, 0]), vec![e(k, EV_KEY, 42, 0), syn(k)]);
    assert_eq!(d.input(&[2, 1]), vec![e(k, EV_KEY, 115, 1), syn(k)]);
    assert_eq!(d.input(&[2, 0]), vec![e(k, EV_KEY, 115, 0), syn(k)]);
}

// Standard HID boot mouse with wheel (HID usage-tables example).
const DESC_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x05, 0x81, 0x03,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06,
    0xC0, 0xC0,
];

#[test]
fn mouse() {
    let mut d = HidDevice::new(0x046d, 0xc077, DESC_MOUSE);
    let m = Role::Mouse;
    assert!(d.has(m));
    assert!(d.input(&[0, 0, 0, 0]).is_empty());
    assert_eq!(d.input(&[1, 0xFB, 3, 0xFF]),
        vec![e(m, EV_KEY, BTN_LEFT, 1), e(m, EV_REL, REL_X, -5), e(m, EV_REL, REL_Y, 3), e(m, EV_REL, REL_WHEEL, -1), syn(m)]);
    assert_eq!(d.input(&[0, 0, 0, 0]), vec![e(m, EV_KEY, BTN_LEFT, 0), syn(m)]);
    assert_eq!(d.input(&[2, 0, 0, 0]), vec![e(m, EV_KEY, BTN_LEFT + 1, 1), syn(m)]);
}

fn de_seq(items: &[&[u8]]) -> Vec<u8> {
    let n: usize = items.iter().map(|i| i.len()).sum();
    let mut v = vec![0x35, n as u8];
    for i in items { v.extend_from_slice(i); }
    v
}

fn pdu(tid: u16, body: &[u8], cont: &[u8]) -> Vec<u8> {
    let mut p = vec![0x07];
    p.extend_from_slice(&tid.to_be_bytes());
    p.extend_from_slice(&((2 + body.len() + 1 + cont.len()) as u16).to_be_bytes());
    p.extend_from_slice(&(body.len() as u16).to_be_bytes());
    p.extend_from_slice(body);
    p.push(cont.len() as u8);
    p.extend_from_slice(cont);
    p
}

#[test]
fn sdp() {
    assert_eq!(sdp_hid_request(1, &[]), vec![
        0x06, 0, 1, 0, 15, 0x35, 3, 0x19, 0x11, 0x24, 0x04, 0x00,
        0x35, 5, 0x0A, 0, 0, 0xFF, 0xFF, 0x00]);
    assert_eq!(sdp_hid_request(2, &[9, 8]).len(), 22);

    let mut text = vec![0x25, DESC_MOUSE.len() as u8];
    text.extend_from_slice(DESC_MOUSE);
    let class_list = de_seq(&[&[0x19, 0x11, 0x24]]);
    let inner = de_seq(&[&[0x08, 0x22], &text]);
    let desc_list = de_seq(&[&inner]);
    let mut rec_items: Vec<u8> = vec![0x09, 0x00, 0x01];
    rec_items.extend_from_slice(&class_list);
    rec_items.extend_from_slice(&[0x09, 0x02, 0x06]);
    rec_items.extend_from_slice(&desc_list);
    let rec = de_seq(&[&rec_items]);
    let lists = de_seq(&[&rec]);

    let (a, b) = lists.split_at(lists.len() / 2);
    let p1 = pdu(5, a, &[0xAA, 0xBB]);
    let p2 = pdu(5, b, &[]);
    let mut acc = Vec::new();
    assert_eq!(sdp_response(&p1, &mut acc), Some(vec![0xAA, 0xBB]));
    assert_eq!(sdp_response(&p2, &mut acc), Some(vec![]));
    assert_eq!(acc, lists);
    assert_eq!(sdp_hid_descriptor(&acc).as_deref(), Some(DESC_MOUSE));
    // error / truncated PDUs
    assert_eq!(sdp_response(&[0x01, 0, 0, 0, 5, 0, 0], &mut Vec::new()), None);
    assert_eq!(sdp_response(&p1[..p1.len() - 1], &mut Vec::new()), None);
    assert_eq!(sdp_hid_descriptor(&lists[..10]), None);
    assert_eq!(sdp_hid_descriptor(&[]), None);
}

#[test]
fn parser_robustness() {
    for desc in [DESC_8BITDO, DESC_KBD, DESC_MOUSE] {
        for n in 0..=desc.len() {
            let mut d = HidDevice::new(1, 2, &desc[..n]);
            let _ = d.input(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
            let _ = d.input(&[3]);
            let _ = d.release_all();
        }
    }
    // long items, including truncated ones
    for d in [&[0xFEu8][..], &[0xFE, 200, 0x10], &[0xFE, 3, 0x10, 1, 2, 3, 0x05, 0x01], &[0xFE, 0xFF]] {
        let _ = parse_descriptor(d);
    }
    let p = parse_descriptor(&[0xFE, 3, 0x10, 1, 2, 3, 0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0xC0]);
    assert_eq!(p.apps, vec![0x010002]);
    // Push/Pop restores the report size
    let p = parse_descriptor(&[
        0x05, 0x01, 0x09, 0x30, 0xA1, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01,
        0xA4, 0x75, 0x10, 0xB4, 0x81, 0x02, 0xC0]);
    assert_eq!(p.fields.len(), 1);
    assert_eq!(p.fields[0].size, 8);
    // Pop on empty stack, absurd counts, unbalanced End Collection
    let _ = parse_descriptor(&[0xB4, 0xC0, 0xC0, 0x96, 0xFF, 0xFF, 0x77, 0xFF, 0xFF, 0xFF, 0xFF, 0x81, 0x02]);
}
