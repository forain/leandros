//! Host tests: `cargo test -p netctl-abi --target aarch64-apple-darwin`
//! (or the host triple of the machine you are on).

extern crate std;
use std::vec::Vec;

use crate::profiles::*;
use crate::types::*;
use crate::util::*;
use crate::*;
use core::mem::offset_of;

// ── Layout: these offsets are the ABI. Changing one is an ABI break. ──────

#[test]
fn command_layouts() {
    assert_eq!(offset_of!(NcVersion, features), 24);
    assert_eq!(offset_of!(NcQuery, gen), 20);
    assert_eq!(offset_of!(NcQuery, buf), 24);
    assert_eq!(offset_of!(NcScan, ssids), 16);
    assert_eq!(offset_of!(NcSsid, bytes), 1);
    assert_eq!(offset_of!(NcConnect, profile_uuid), 16);
    assert_eq!(offset_of!(NcConnect, bssid), 32);
    assert_eq!(offset_of!(NcConnect, ssid_len), 38);
    assert_eq!(offset_of!(NcConnect, akm), 39);
    assert_eq!(offset_of!(NcConnect, pmf), 40);
    assert_eq!(offset_of!(NcConnect, secret_len), 42);
    assert_eq!(offset_of!(NcConnect, ssid), 44);
    assert_eq!(offset_of!(NcConnect, secret), 80);
    assert_eq!(offset_of!(NcReload, source), 12);
    assert_eq!(offset_of!(NcRadio, on), 8);
}

#[test]
fn record_layouts() {
    assert_eq!(offset_of!(NcIface, name), 8);
    assert_eq!(offset_of!(NcIface, mac), 24);
    assert_eq!(offset_of!(NcIface, flags), 32);
    assert_eq!(offset_of!(NcIface, wifi_caps), 44);
    assert_eq!(offset_of!(NcIface, perm_mac), 48);
    assert_eq!(offset_of!(NcIface, link_gen), 56);
    assert_eq!(offset_of!(NcIface, driver), 64);
    assert_eq!(offset_of!(NcIpv4, addr), 8);
    assert_eq!(offset_of!(NcIpv4, dns), 16);
    assert_eq!(offset_of!(NcIpv4, metric), 32);
    assert_eq!(offset_of!(NcIpv4, method), 40);
    assert_eq!(offset_of!(NcWifiState, attempt), 12);
    assert_eq!(offset_of!(NcWifiState, profile_uuid), 16);
    assert_eq!(offset_of!(NcWifiState, bssid), 32);
    assert_eq!(offset_of!(NcWifiState, signal_dbm), 40);
    assert_eq!(offset_of!(NcWifiState, ssid), 44);
    assert_eq!(offset_of!(NcWifiState, flags), 84);
    assert_eq!(offset_of!(NcBss, ssid), 16);
    assert_eq!(offset_of!(NcBss, nm_flags), 48);
    assert_eq!(offset_of!(NcBss, rsn_flags), 56);
    assert_eq!(offset_of!(NcBss, ext), 60);
    assert_eq!(offset_of!(NcBss, last_seen_ms), 80);
    assert_eq!(offset_of!(NcProfileMeta, priority), 20);
    assert_eq!(offset_of!(NcProfileMeta, ssid), 32);
    assert_eq!(offset_of!(NcProfileMeta, id), 64);
    assert_eq!(offset_of!(NcProfileMeta, iface), 128);
    assert_eq!(offset_of!(NcProfileMeta, timestamp_s), 144);
    assert_eq!(offset_of!(NcScanInfo, time_ms), 16);
    assert_eq!(offset_of!(NcProfilesInfo, seq), 16);
    assert_eq!(offset_of!(NcEventRecord, ifindex), 4);
    assert_eq!(offset_of!(NcEventRecord, seq), 8);
    assert_eq!(offset_of!(NcEventRecord, time_ms), 12);
    assert_eq!(offset_of!(NcEventRecord, payload), 16);
}

#[test]
fn ioctl_numbers() {
    // Pinned values: userland may hard-code them.
    assert_eq!(NETCTL_GET_VERSION, 0x8020_4E00);
    assert_eq!(NETCTL_GET_IFACES, 0xC020_4E01);
    assert_eq!(NETCTL_GET_PROFILES, 0xC020_4E05);
    assert_eq!(NETCTL_REQUEST_SNAPSHOT, 0x0000_4E06);
    assert_eq!(NETCTL_SCAN, 0x40A0_4E20);
    assert_eq!(NETCTL_CONNECT, 0xC0D0_4E21);
    assert_eq!(NETCTL_DISCONNECT, 0x4010_4E22);
    assert_eq!(NETCTL_SET_RADIO, 0x4010_4E23);
    assert_eq!(NETCTL_RELOAD_PROFILES, 0xC010_4E24);
    for c in [NETCTL_GET_VERSION, NETCTL_GET_IFACES, NETCTL_GET_IPV4, NETCTL_GET_WIFI, NETCTL_GET_BSS,
              NETCTL_GET_PROFILES, NETCTL_REQUEST_SNAPSHOT] {
        assert!(!is_mutating(c) && is_netctl_ioctl(c));
    }
    for c in [NETCTL_SCAN, NETCTL_CONNECT, NETCTL_DISCONNECT, NETCTL_SET_RADIO, NETCTL_RELOAD_PROFILES] {
        assert!(is_mutating(c));
    }
    assert_eq!(ioc_size(NETCTL_CONNECT), NcConnect::SIZE);
    assert!(!is_mutating(0x8020_4400)); // another driver's 0x20
}

#[test]
fn cmd_new_sets_size() {
    assert_eq!(NcConnect::new().size, 208);
    assert_eq!(NcQuery::new().size, 32);
    assert_eq!(NcScan::new().size, 160);
}

#[test]
fn read_prefix_zero_extends() {
    let mut q = NcQuery::new();
    q.ifindex = 3;
    let short = &q.as_bytes()[..8];
    let r = NcQuery::read_prefix(short);
    assert_eq!(r.size, 32);
    assert_eq!(r.ifindex, 3);
    assert_eq!(r.buf, 0);
    assert!(NcQuery::read_from(short).is_none());
}

// ── Events ─────────────────────────────────────────────────────────────────

fn sample_bss() -> NcBss {
    let mut b = NcBss::default();
    b.ifindex = 3;
    b.bssid = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
    b.freq_mhz = 5180;
    b.signal_dbm = -61;
    b.quality = quality_from_dbm(-61);
    b.ssid_len = 32;
    b.ssid = *b"0123456789abcdef0123456789ABCDEF";
    b.nm_flags = nm_ap_flags::PRIVACY;
    b.rsn_flags = nm_sec::KEY_MGMT_PSK | nm_sec::KEY_MGMT_SAE | nm_sec::PAIR_CCMP | nm_sec::GROUP_CCMP;
    b.ext = bss_ext::AKM_PSK | bss_ext::AKM_SAE | bss_ext::TRANSITION | bss_ext::MFPC | bss_ext::JOINABLE;
    b.last_seen_ms = 123_456;
    b
}

fn sample_profile_meta() -> NcProfileMeta {
    let p = Profile::new_wifi([7; 16], b"Home", akm::AUTO, b"correct horse").unwrap();
    let mut m = p.meta(5, 0);
    m.id_len = 64;
    m.id = [b'x'; 64];
    m
}

fn roundtrip(ev: Event) -> (EventMeta, Event, usize) {
    let enc = ev.encode(0, 42, 1000);
    let mut buf = [0u8; EVENT_RECORD_SIZE * EVENT_MAX_PARTS];
    let n = enc.write_to(&mut buf).unwrap();
    let mut r = Reassembler::new();
    let mut out = None;
    for rec in records(&buf[..n]) {
        assert!(out.is_none(), "event completed before its last part");
        out = r.push(&rec).unwrap();
    }
    let (m, e) = out.expect("incomplete");
    (m, e, enc.len())
}

#[test]
fn event_roundtrips_and_parts() {
    let cases: Vec<(Event, usize)> = std::vec![
        (Event::SnapshotBegin(NcSnapshotInfo { reason: snapshot_reason::OPEN, abi_version: 1, ..Default::default() }), 1),
        (Event::Resync(NcResync { dropped: 9, ..Default::default() }), 1),
        (Event::IfaceAdded(NcIface { ifindex: 2, kind: if_kind::ETHERNET, ..Default::default() }), 2),
        (Event::Ipv4(NcIpv4 { ifindex: 2, state: ip_state::BOUND, addr: [10, 0, 2, 15], prefix: 24, ..Default::default() }), 1),
        (Event::WifiState(NcWifiState { ifindex: 3, state: wifi_state::FAILED, reason: reason::WRONG_KEY, ..Default::default() }), 2),
        (Event::WifiSignal(NcWifiSignal { ifindex: 3, signal_dbm: -70, ..Default::default() }), 1),
        (Event::ScanDone(NcScanInfo { ifindex: 3, n_bss: 4, time_ms: 1 << 40, ..Default::default() }), 1),
        (Event::Bss(sample_bss()), 2),
        (Event::BssGone(sample_bss()), 2),
        (Event::Profile(sample_profile_meta()), 4),
        (Event::ProfilesEnd(NcProfilesInfo { gen: 5, count: 1, seq: 77, ..Default::default() }), 1),
    ];
    for (ev, parts) in cases {
        assert_eq!(kind_parts(ev.kind()), Some(parts));
        let (m, back, n) = roundtrip(ev);
        assert_eq!(n, parts);
        assert_eq!(back, ev);
        assert_eq!(m.kind, ev.kind());
        assert_eq!(m.ifindex, ev.ifindex());
        assert_eq!(m.seq, 42);
        assert_eq!(m.time_ms, 1000);
    }
}

#[test]
fn ssid_of_32_bytes_survives_the_split() {
    let (_, back, n) = roundtrip(Event::Bss(sample_bss()));
    assert_eq!(n, 2);
    match back {
        Event::Bss(b) => assert_eq!(b.ssid(), b"0123456789abcdef0123456789ABCDEF"),
        _ => panic!(),
    }
    let mut ws = NcWifiState::default();
    ws.ssid_len = 32;
    ws.ssid = [0xFF; 32]; // arbitrary bytes, not UTF-8
    let (_, back, _) = roundtrip(Event::WifiState(ws));
    assert_eq!(back, Event::WifiState(ws));
}

#[test]
fn header_fields_on_every_part() {
    let enc = Event::Profile(sample_profile_meta()).encode(event_flags::SNAPSHOT, 7, 9);
    for (i, r) in enc.records().iter().enumerate() {
        assert_eq!((r.kind, r.flags, r.part as usize, r.nparts, r.seq, r.time_ms),
                   (kind::PROFILE, event_flags::SNAPSHOT, i, 4, 7, 9));
    }
}

#[test]
fn reassembler_drops_partial_on_resync() {
    let bss = Event::Bss(sample_bss()).encode(0, 10, 0);
    let resync = Event::Resync(NcResync { dropped: 3, ..Default::default() }).encode(0, 14, 0);
    let mut r = Reassembler::new();
    assert_eq!(r.push(&bss.records()[0]).unwrap(), None);
    assert!(r.in_progress());
    // Overflow cut the event: RESYNC arrives instead of part 1.
    let (m, e) = r.push(&resync.records()[0]).unwrap().unwrap();
    assert_eq!(m.kind, kind::RESYNC);
    assert_eq!(e, Event::Resync(NcResync { dropped: 3, ..Default::default() }));
    assert_eq!(r.dropped_partials, 1);
    // A stray continuation is rejected and does not poison what follows.
    assert_eq!(r.push(&bss.records()[1]), Err(DecodeError::OrphanPart));
    let (_, e) = {
        assert_eq!(r.push(&bss.records()[0]).unwrap(), None);
        r.push(&bss.records()[1]).unwrap().unwrap()
    };
    assert_eq!(e, Event::Bss(sample_bss()));
}

#[test]
fn reassembler_rejects_bad_framing_and_mismatched_parts() {
    let mut r = Reassembler::new();
    let mut rec = NcEventRecord::default();
    rec.nparts = 0;
    assert_eq!(r.push(&rec), Err(DecodeError::BadFraming));
    rec.nparts = 9;
    assert_eq!(r.push(&rec), Err(DecodeError::BadFraming));
    rec.nparts = 2;
    rec.part = 2;
    assert_eq!(r.push(&rec), Err(DecodeError::BadFraming));
    // Part 1 of a different seq after part 0: orphan, partial dropped.
    let a = Event::Bss(sample_bss()).encode(0, 1, 0);
    let b = Event::Bss(sample_bss()).encode(0, 2, 0);
    assert_eq!(r.push(&a.records()[0]).unwrap(), None);
    assert_eq!(r.push(&b.records()[1]), Err(DecodeError::OrphanPart));
    assert!(!r.in_progress());
}

#[test]
fn unknown_kind_is_skipped_whole() {
    // A newer kernel's 3-part event of an unknown kind, then a known one.
    let payload = [0xAB; 3 * EVENT_PAYLOAD_SIZE];
    let unk = encode_raw(0x7E, 0, 0, 5, 0, &payload);
    let ipv4 = Event::Ipv4(NcIpv4 { ifindex: 2, ..Default::default() }).encode(0, 6, 0);
    let mut r = Reassembler::new();
    let mut got = Vec::new();
    for rec in unk.records().iter().chain(ipv4.records()) {
        if let Some((_, e)) = r.push(rec).unwrap() { got.push(e); }
    }
    assert_eq!(got.len(), 2);
    assert_eq!(got[0], Event::Unknown { kind: 0x7E });
    assert!(matches!(got[1], Event::Ipv4(_)));
}

#[test]
fn growth_tolerated_both_ways() {
    // Newer kernel: NcIpv4 grew to 2 parts; old reader takes the prefix.
    let mut p = [0u8; 2 * EVENT_PAYLOAD_SIZE];
    let v4 = NcIpv4 { ifindex: 4, prefix: 16, ..Default::default() };
    p[..48].copy_from_slice(v4.as_bytes());
    p[48..].fill(0xEE);
    let enc = encode_raw(kind::IPV4, 0, 4, 1, 0, &p);
    let mut r = Reassembler::new();
    assert_eq!(r.push(&enc.records()[0]).unwrap(), None);
    let (_, e) = r.push(&enc.records()[1]).unwrap().unwrap();
    assert_eq!(e, Event::Ipv4(v4));
    // Older kernel: a 1-part NcBss; the reader zero-extends.
    let enc = encode_raw(kind::BSS, 0, 3, 1, 0, &sample_bss().as_bytes()[..48]);
    let (_, e) = Reassembler::new().push(&enc.records()[0]).unwrap().unwrap();
    match e { Event::Bss(b) => { assert_eq!(b.ssid_len, 32); assert_eq!(b.ext, 0); } _ => panic!() }
}

#[test]
fn max_snapshot_fits_the_queue() {
    // Worst case: MAX_IFACES interfaces, all Wi-Fi (state + last SCAN_DONE
    // each), the kernel-wide MAX_BSS cache, MAX_PROFILES profiles.
    let p = |k| kind_parts(k).unwrap();
    let per_if = p(kind::IFACE_ADDED) + p(kind::IPV4) + p(kind::WIFI_STATE) + p(kind::SCAN_DONE);
    let max = p(kind::SNAPSHOT_BEGIN) + MAX_IFACES * per_if + MAX_BSS * p(kind::BSS)
        + MAX_PROFILES * p(kind::PROFILE) + p(kind::PROFILES_END) + p(kind::SNAPSHOT_END)
        + p(kind::RESYNC);
    assert_eq!(max, 564);
    assert!(max <= EVENT_QUEUE_RECORDS);
}

// ── Profiles file ──────────────────────────────────────────────────────────

fn profs() -> Vec<Profile> {
    let mut a = Profile::new_wifi([1; 16], b"Home", akm::AUTO, b"hunter2hunter2").unwrap();
    a.priority = 10;
    a.timestamp_s = 1_765_000_000;
    let mut b = Profile::new_wifi([2; 16], &[0xC3, 0xA9, 0, 0xFF], akm::PSK,
        b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
    b.flags = profile_flags::HIDDEN;
    b.autoconnect = false;
    b.set_id("Café net".as_bytes());
    crate::util::set_cstr(&mut b.iface, b"wifi0");
    let c = Profile::new_wifi([3; 16], b"Open", akm::NONE, b"").unwrap();
    let mut d = Profile::new_wifi([4; 16], b"WPA3", akm::SAE, &[b'p'; 100]).unwrap();
    d.pmf = pmf::REQUIRED;
    std::vec![a, b, c, d]
}

fn image(seq: u64, flags: u32, p: &[Profile]) -> Vec<u8> {
    let mut out = std::vec![0u8; serialized_len(p.len())];
    let n = serialize(seq, flags, p, &mut out).unwrap();
    assert_eq!(n, out.len());
    out
}

#[test]
fn crc32_check_value() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
}

#[test]
fn profiles_roundtrip() {
    let ps = profs();
    let img = image(42, global_flags::WIFI_RADIO_OFF, &ps);
    assert_eq!(img.len(), 64 + 4 * 320 + 4);
    let v = parse_slot(&img).unwrap();
    assert_eq!(v.seq, 42);
    assert_eq!(v.global_flags, global_flags::WIFI_RADIO_OFF);
    let back: Vec<Profile> = v.profiles().collect();
    assert_eq!(back, ps);
    assert_eq!(back[1].iface(), b"wifi0");
    assert_eq!(back[1].id(), "Café net".as_bytes());
    assert!(back[1].hidden());
    // Meta never carries the secret, but says one exists.
    let m = back[0].meta(3, profile_flags::LEGACY);
    assert_eq!(m.flags, profile_flags::HAS_SECRET | profile_flags::LEGACY);
    assert!(!m.as_bytes().windows(14).any(|w| w == b"hunter2hunter2"));
    assert_eq!(back[2].meta(3, 0).flags & profile_flags::HAS_SECRET, 0);
    // Debug output must not leak the secret either.
    let dbg = std::format!("{:?}", back[0]);
    assert!(!dbg.contains("hunter2"));
}

#[test]
fn empty_slot_is_valid() {
    let img = image(1, 0, &[]);
    let v = parse_slot(&img).unwrap();
    assert_eq!(v.count, 0);
    assert_eq!(v.profiles().count(), 0);
}

#[test]
fn torn_and_corrupt_slots_are_rejected() {
    let img = image(5, 0, &profs());
    for cut in [0, 10, 63, 64, 67, 64 + 320, img.len() - 1] {
        let e = parse_slot(&img[..cut]).unwrap_err();
        assert!(matches!(e, SlotError::TooShort | SlotError::Truncated), "cut {cut}: {e:?}");
    }
    // Every single-bit flip anywhere is caught.
    for i in 0..img.len() {
        let mut bad = img.clone();
        bad[i] ^= 0x10;
        assert!(parse_slot(&bad).is_err(), "flip at {i} accepted");
    }
    let mut bad = img.clone();
    bad[8] = 2;
    assert_eq!(parse_slot(&bad).unwrap_err(), SlotError::BadVersion);
    let mut bad = img.clone();
    bad[12..14].copy_from_slice(&100u16.to_le_bytes());
    assert_eq!(parse_slot(&bad).unwrap_err(), SlotError::BadGeometry);
    // Trailing bytes after the CRC (a longer old file, no O_TRUNC) are fine.
    let mut long = img.clone();
    long.extend_from_slice(&[0x55; 300]);
    assert_eq!(parse_slot(&long).unwrap().seq, 5);
}

#[test]
fn newest_valid_slot_wins() {
    let old = image(7, 0, &profs()[..1]);
    let new = image(8, 0, &profs());
    let (s, v) = select(Some(&old), Some(&new)).unwrap();
    assert_eq!((s, v.seq, v.count), (Slot::B, 8, 4));
    let (s, v) = select(Some(&new), Some(&old)).unwrap();
    assert_eq!((s, v.seq), (Slot::A, 8));
    // Torn newer slot: the older one stays in charge.
    let torn = &new[..new.len() - 100];
    let (s, v) = select(Some(&old), Some(torn)).unwrap();
    assert_eq!((s, v.seq), (Slot::A, 7));
    let mut corrupt = new.clone();
    corrupt[200] ^= 1;
    assert_eq!(select(Some(&corrupt), Some(&old)).unwrap().0, Slot::B);
    assert!(select(Some(torn), None).is_none());
    assert!(select(None, None).is_none());
    // Tie: A.
    assert_eq!(select(Some(&old), Some(&old)).unwrap().0, Slot::A);
}

#[test]
fn writer_alternates_slots() {
    assert_eq!(write_target(None, None), (Slot::A, 1));
    let a1 = image(1, 0, &[]);
    assert_eq!(write_target(Some(&a1), None), (Slot::B, 2));
    let b2 = image(2, 0, &[]);
    assert_eq!(write_target(Some(&a1), Some(&b2)), (Slot::A, 3));
    // Newest is torn: overwrite the torn one, keep the good older one.
    assert_eq!(write_target(Some(&a1), Some(&b2[..20])), (Slot::B, 2));
}

#[test]
fn bad_entries_are_skipped_not_fatal() {
    let ps = profs();
    let mut img = image(9, 0, &ps);
    let e = |i: usize| HEADER_SIZE + i * ENTRY_SIZE;
    img[e(0) + 17] = 99;                    // unknown akm
    img[e(1) + 24] = 33;                    // ssid too long
    img[e(2)..e(2) + 16].copy_from_slice(&[4; 16]); // duplicate of entry 3's uuid… but entry 2 comes first
    let body = img.len() - 4;
    let c = crc32(&img[..body]);
    img[body..].copy_from_slice(&c.to_le_bytes());
    let v = parse_slot(&img).unwrap();
    let r: Vec<_> = v.entries().collect();
    assert_eq!(r[0].as_ref().unwrap_err(), &EntryError::UnknownAkm);
    assert_eq!(r[1].as_ref().unwrap_err(), &EntryError::BadSsidLen);
    assert_eq!(r[2].as_ref().unwrap().ssid(), b"Open");
    assert_eq!(r[3].as_ref().unwrap_err(), &EntryError::DuplicateUuid);
    assert_eq!(v.profiles().count(), 1);
}

#[test]
fn larger_entries_from_a_newer_writer() {
    // entry_size 400 and header_size 80: a v1 reader still parses it.
    let ps = profs();
    let (hs, es) = (80usize, 400usize);
    let mut img = std::vec![0u8; hs + ps.len() * es + 4];
    img[0..8].copy_from_slice(&MAGIC);
    img[8..10].copy_from_slice(&1u16.to_le_bytes());
    img[10..12].copy_from_slice(&(hs as u16).to_le_bytes());
    img[12..14].copy_from_slice(&(es as u16).to_le_bytes());
    img[14..16].copy_from_slice(&(ps.len() as u16).to_le_bytes());
    img[16..24].copy_from_slice(&3u64.to_le_bytes());
    for (i, p) in ps.iter().enumerate() {
        let mut e = [0u8; ENTRY_SIZE];
        p.encode(&mut e);
        img[hs + i * es..hs + i * es + ENTRY_SIZE].copy_from_slice(&e);
        img[hs + i * es + ENTRY_SIZE..hs + (i + 1) * es].fill(0x77);
    }
    let body = img.len() - 4;
    let c = crc32(&img[..body]);
    img[body..].copy_from_slice(&c.to_le_bytes());
    let back: Vec<Profile> = parse_slot(&img).unwrap().profiles().collect();
    assert_eq!(back, ps);
}

#[test]
fn serialize_validates() {
    let mut out = std::vec![0u8; FILE_MAX];
    let dup = std::vec![profs()[0].clone(), profs()[0].clone()];
    assert_eq!(serialize(1, 0, &dup, &mut out), Err(SerError::Invalid(EntryError::DuplicateUuid)));
    let mut bad = profs()[0].clone();
    bad.akm = akm::PSK;
    bad.secret_len = 5;
    assert_eq!(serialize(1, 0, &[bad], &mut out), Err(SerError::Invalid(EntryError::BadSecret)));
    assert_eq!(serialize(1, 0, &profs(), &mut out[..100]), Err(SerError::BufferTooSmall));
    assert!(Profile::new_wifi([0; 16], b"x", akm::NONE, b"").is_none()); // nil uuid
    assert!(Profile::new_wifi([1; 16], b"", akm::NONE, b"").is_none());
    assert!(Profile::new_wifi([1; 16], b"x", akm::NONE, b"secret12").is_none());
}

#[test]
fn shortened_values_leave_no_stale_bytes() {
    let mut p = profs()[0].clone();
    let mut e = [0xAAu8; ENTRY_SIZE];
    p.encode(&mut e);
    p.wipe_secret();
    p.encode(&mut e);
    assert!(e[160..288].iter().all(|&b| b == 0));
}

#[test]
fn legacy_profile_from_wifi_conf() {
    let conf = b"# home\nssid=\"My Net\"\npsk=  passw0rd!  \n";
    let (s, k) = parse_legacy_wifi_conf(conf).unwrap();
    assert_eq!((s, k), (&b"My Net"[..], &b"passw0rd!"[..]));
    let p = legacy_profile(s, k).unwrap();
    assert_eq!(p.akm, akm::AUTO);
    assert_eq!(p.priority, 0);
    assert_eq!(p.uuid, legacy_profile(s, b"other-pass").unwrap().uuid); // keyed by SSID only
    assert_eq!(legacy_profile(b"Cafe", b"").unwrap().akm, akm::NONE);
    assert!(legacy_profile(b"Cafe", b"short").is_none());
    assert!(parse_legacy_wifi_conf(b"psk=x\n").is_none());
}

// ── Helpers ────────────────────────────────────────────────────────────────

#[test]
fn quality_matches_networkmanager() {
    assert_eq!(quality_from_dbm(-40), 100);
    assert_eq!(quality_from_dbm(-30), 100);
    assert_eq!(quality_from_dbm(-70), 50);
    assert_eq!(quality_from_dbm(-100), 0);
    assert_eq!(quality_from_dbm(-120), 0);
}

#[test]
fn channels_and_frequencies() {
    assert_eq!(channel_to_freq(1, band::GHZ_2_4), 2412);
    assert_eq!(channel_to_freq(14, band::GHZ_2_4), 2484);
    assert_eq!(channel_to_freq(36, band::GHZ_5), 5180);
    assert_eq!(channel_to_freq(1, band::GHZ_6), 5955);
    for (ch, b) in [(1, band::GHZ_2_4), (13, band::GHZ_2_4), (14, band::GHZ_2_4), (36, band::GHZ_5),
                    (165, band::GHZ_5), (1, band::GHZ_6), (2, band::GHZ_6), (233, band::GHZ_6)] {
        assert_eq!(freq_to_channel(channel_to_freq(ch, b)), (ch, b));
    }
}

#[test]
fn uuids() {
    let u = derive_uuid(UUID_DOMAIN_WIRED, &[0x52, 0x54, 0, 0x12, 0x34, 0x56]);
    assert_eq!(u[6] >> 4, 8);
    assert_eq!(u[8] >> 6, 2);
    assert_ne!(u, derive_uuid(UUID_DOMAIN_LEGACY, &[0x52, 0x54, 0, 0x12, 0x34, 0x56]));
    let s = uuid_to_str(&u);
    assert_eq!(s[8], b'-');
    assert_eq!(uuid_from_str(&s), Some(u));
    assert_eq!(&uuid_to_str(&[0xAB; 16]), b"abababab-abab-abab-abab-abababababab");
    assert_eq!(uuid_from_str(b"ABABABAB-ABAB-ABAB-ABAB-ABABABABABAB"), Some([0xAB; 16]));
    assert!(uuid_from_str(b"abababab-abab-abab-abab-ababababab").is_none());
    assert!(uuid_from_str(b"abababababab-abab-abab-abababababab").is_none());
}

#[test]
fn secrets() {
    assert!(secret_valid_for(akm::PSK, b"12345678"));
    assert!(!secret_valid_for(akm::PSK, b"1234567"));
    assert!(!secret_valid_for(akm::PSK, &[b'a'; 65]));
    assert!(secret_valid_for(akm::PSK, &[b'a'; 64]));
    assert!(!secret_valid_for(akm::PSK, &[b'g'; 64]));
    assert!(secret_valid_for(akm::SAE, b"x"));
    assert!(secret_valid_for(akm::AUTO, &[b'z'; 128]));
    assert!(!secret_valid_for(akm::AUTO, &[b'z'; 129]));
    assert!(!secret_valid_for(akm::NONE, b"12345678"));
    assert!(secret_valid_for(akm::NONE, b""));
    let mut c = NcConnect::new();
    c.secret_len = 3;
    c.secret[..3].copy_from_slice(b"abc");
    c.wipe_secret();
    assert_eq!(c.secret(), b"");
    assert!(c.secret.iter().all(|&b| b == 0));
}

#[test]
fn stamp_policy() {
    use crate::profiles::{stamp, Profile};
    let mut p = Profile::new_wifi([1; 16], b"x", 0, b"").unwrap();
    assert!(stamp(&mut p, 50) && p.timestamp_s == 1, "no clock: proven, time unknown");
    assert!(!stamp(&mut p, 60));
    assert!(stamp(&mut p, 1_800_000_000) && p.timestamp_s == 1_800_000_000, "clock set replaces 1");
    assert!(!stamp(&mut p, 1_800_000_000 + 3600), "within 6 h: no write");
    assert!(stamp(&mut p, 1_800_000_000 + 7 * 3600));
}
