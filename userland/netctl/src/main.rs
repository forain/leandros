//! netctl — the `/dev/netctl` command-line client.
//!
//! ```text
//! netctl [status]                      interfaces, IPv4, Wi-Fi state
//! netctl ifaces                        interface list
//! netctl scan [IF]                     scan (W), wait for SCAN_DONE, list BSSs
//! netctl bss [IF]                      cached scan results
//! netctl connect SSID [PASS] [opts]    inline connect (W); opts: --if IF
//!                                      --bssid MAC --hidden --akm auto|psk|psk-sha256|sae|none
//! netctl connect --uuid UUID [--if IF] connect a saved profile (W)
//! netctl disconnect [IF] [--keep-auto] (W); on a wired IF: admin down
//! netctl up IF                         wired admin up: DHCP again (W)
//! netctl radio on|off [IF]             soft rfkill (W)
//! netctl profiles                      saved profiles (no secrets)
//! netctl save SSID [PASS] [--prio N] [--hidden] [--no-auto]
//!                                      write a profile to the slots, reload (W)
//! netctl forget SSID|UUID              remove a saved profile, reload (W)
//! netctl reload                        re-read the profile slots (W)
//! netctl watch [--no-snapshot] [--for S]  print events as they arrive (for S seconds)
//! ```
//!
//! (W): needs a write-capable open, i.e. root or the `netdev` group.
//! Exit status: 0 ok, 1 error, 2 usage.

#![no_std]
#![no_main]

extern crate alloc;
extern crate leandros_libc;

#[macro_use]
#[path = "common.rs"]
mod common;

use common::*;
use netctl_abi::profiles::Profile;
use netctl_abi::*;

fn usage() -> i32 {
    eprintln!("usage: netctl [status|ifaces|scan [IF]|bss [IF]|connect SSID [PASS] [--if IF] [--bssid MAC] [--hidden] [--akm A]|connect --uuid UUID|disconnect [IF] [--keep-auto]|up IF|radio on|off [IF]|profiles|save SSID [PASS] [--prio N] [--hidden] [--no-auto]|forget SSID|UUID|reload|watch [--no-snapshot] [--for S]]");
    2
}

fn fail(what: &str, e: i32) -> i32 {
    eprintln!("netctl: {}: {} ({})", what, errname(e), e);
    if e == EACCES || e == EPERM {
        eprintln!("netctl: managing the network needs root or the 'netdev' group");
    }
    1
}

fn open_rw() -> Result<Netctl, i32> { Netctl::open(O_RDWR) }
fn open_ro() -> Result<Netctl, i32> { Netctl::open(O_RDONLY) }

fn print_iface(i: &NcIface) {
    say!("{}: {} <{}> {}", i.ifindex, name_of(i), kind_name(i.kind), oper_name(i.oper));
    if i.kind != if_kind::LOOPBACK { say!(" mac {}", Mac(i.mac)); }
    let f = i.flags;
    say!(" flags");
    for (b, n) in [(iface_flags::ADMIN_UP, "admin-up"), (iface_flags::CARRIER, "carrier"),
                   (iface_flags::RADIO_ON, "radio-on"), (iface_flags::RADIO_HW_ON, "hw-on"),
                   (iface_flags::READY, "ready")] {
        if f & b != 0 { say!(" {}", n); }
    }
    if i.speed_mbps != 0 { say!(" speed {}Mb/s", i.speed_mbps); }
    if i.metric != 0 { say!(" metric {}", i.metric); }
    sayln!(" driver {}", core::str::from_utf8(i.driver()).unwrap_or("?"));
}

fn print_ipv4(v: &NcIpv4) {
    say!("    ipv4 {}", ip_state_name(v.state));
    if v.state == ip_state::BOUND || v.state == ip_state::STATIC {
        say!(" {}/{}", Ip(v.addr), v.prefix);
        if v.flags & ip_flags::HAS_GW != 0 { say!(" gw {}", Ip(v.gw)); }
        if v.n_dns > 0 {
            say!(" dns");
            for d in &v.dns[..(v.n_dns as usize).min(3)] { say!(" {}", Ip(*d)); }
        }
        if v.flags & ip_flags::DEFAULT_ROUTE != 0 { say!(" (default route)"); }
    }
    sayln!("");
}

fn print_wifi(w: &NcWifiState) {
    say!("    wifi {}", wifi_state_name(w.state));
    if w.reason != 0 { say!(" reason {}", reason_name(w.reason)); }
    if w.ssid_len > 0 { say!(" ssid \"{}\"", Esc(w.ssid())); }
    if w.bssid != [0; 6] { say!(" bssid {}", Mac(w.bssid)); }
    if w.freq_mhz != 0 { say!(" {}MHz", w.freq_mhz); }
    if w.signal_dbm != 0 { say!(" {}dBm", w.signal_dbm); }
    if w.akm != akm::AUTO { say!(" akm {}", akm_name(w.akm)); }
    if w.attempt != 0 { say!(" attempt {}", w.attempt); }
    sayln!("");
}

fn print_bss(b: &NcBss) {
    let sec = if b.rsn_flags & nm_sec::KEY_MGMT_SAE != 0 && b.rsn_flags & nm_sec::KEY_MGMT_PSK != 0 { "wpa2/wpa3" }
        else if b.rsn_flags & nm_sec::KEY_MGMT_SAE != 0 { "wpa3" }
        else if b.rsn_flags & nm_sec::KEY_MGMT_PSK != 0 { "wpa2" }
        else if b.rsn_flags & nm_sec::KEY_MGMT_802_1X != 0 { "802.1x" }
        else if b.wpa_flags != 0 { "wpa1" }
        else if b.nm_flags & nm_ap_flags::PRIVACY != 0 { "wep" }
        else { "open" };
    sayln!("  {} {:>4}MHz {:>4}dBm {:>3}% {:<9} {}{}", Mac(b.bssid), b.freq_mhz, b.signal_dbm, b.quality, sec,
           Esc(b.ssid()), if b.ext & bss_ext::JOINABLE == 0 { "  (not joinable)" } else { "" });
}

fn status(only_ifaces: bool) -> i32 {
    let n = match open_ro() { Ok(n) => n, Err(e) => return fail("open /dev/netctl", e) };
    if !only_ifaces {
        match n.version() {
            Ok(v) => sayln!("netctl ABI {} (features 0x{:x})", v.abi_version, v.features),
            Err(e) => return fail("version", e),
        }
    }
    let mut ifs = [NcIface::default(); MAX_IFACES];
    let k = match n.ifaces(&mut ifs) { Ok(k) => k, Err(e) => return fail("ifaces", e) };
    let mut v4 = [NcIpv4::default(); MAX_IFACES];
    let (n4, _, _) = n.query(NETCTL_GET_IPV4, 0, &mut v4).unwrap_or((0, 0, 0));
    let mut wf = [NcWifiState::default(); MAX_IFACES];
    let (nw, _, _) = n.query(NETCTL_GET_WIFI, 0, &mut wf).unwrap_or((0, 0, 0));
    for i in &ifs[..k] {
        print_iface(i);
        if only_ifaces { continue; }
        for v in v4[..n4].iter().filter(|v| v.ifindex == i.ifindex) { print_ipv4(v); }
        for w in wf[..nw].iter().filter(|w| w.ifindex == i.ifindex) { print_wifi(w); }
    }
    0
}

fn list_bss(n: &Netctl, ifindex: u32) -> i32 {
    let mut b = alloc::vec![NcBss::default(); MAX_BSS];
    match n.query(NETCTL_GET_BSS, ifindex, &mut b[..]) {
        Ok((k, count, gen)) => {
            sayln!("{} BSS (scan generation {})", count, gen);
            for x in &b[..k] { print_bss(x); }
            0
        }
        Err(e) => fail("bss", e),
    }
}

fn scan(ifarg: Option<&[u8]>) -> i32 {
    let n = match open_rw() { Ok(n) => n, Err(e) => return fail("open /dev/netctl for writing", e) };
    let ix = match ifarg { Some(a) => match n.resolve_if(a) { Ok(i) => i, Err(e) => return fail("interface", e) }, None => 0 };
    let mut s = NcScan::new();
    s.ifindex = ix;
    if let Err(e) = n.ioctl(NETCTL_SCAN, &mut s) { return fail("scan", e); }
    // Wait for SCAN_DONE (after the snapshot, which may carry an old one).
    let mut r = Reassembler::new();
    let mut buf = [0u8; 64 * 32];
    let deadline = now_ms() + 15_000;
    let mut live = false;
    let mut done = false;
    while !done && now_ms() < deadline {
        match poll1(n.fd, 500) { Ok(0) => continue, Ok(_) => {}, Err(e) => return fail("poll", e) }
        let k = match n.read_records(&mut buf) { Ok(k) => k, Err(EAGAIN) => continue, Err(e) => return fail("read", e) };
        for_events(&mut r, &buf, k, |m, e| match e {
            Event::SnapshotEnd(_) => live = true,
            Event::ScanDone(i) if live && !m.is_snapshot() && (ix == 0 || i.ifindex == ix) => done = true,
            _ => {}
        });
    }
    if !done { eprintln!("netctl: no SCAN_DONE within 15 s; cached results:"); }
    list_bss(&n, ix)
}

fn parse_mac(s: &[u8]) -> Option<[u8; 6]> {
    let mut m = [0u8; 6];
    let mut it = s.split(|&c| c == b':');
    for b in m.iter_mut() {
        let p = it.next()?;
        if p.len() != 2 { return None; }
        let h = |c: u8| (c as char).to_digit(16).map(|v| v as u8);
        *b = (h(p[0])? << 4) | h(p[1])?;
    }
    if it.next().is_some() { None } else { Some(m) }
}

fn parse_akm(s: &[u8]) -> Option<u8> {
    Some(match s { b"auto" => akm::AUTO, b"none" | b"open" => akm::NONE, b"psk" => akm::PSK,
                   b"psk-sha256" => akm::PSK_SHA256, b"sae" => akm::SAE, _ => return None })
}

fn saved_profile_for(ssid: &[u8]) -> Option<[u8; 16]> {
    let n = open_ro().ok()?;
    let mut p = alloc::vec![NcProfileMeta::default(); MAX_PROFILES];
    let (k, _, _) = n.query(NETCTL_GET_PROFILES, 0, &mut p[..]).ok()?;
    p[..k].iter().filter(|m| m.ssid() == ssid).map(|m| m.uuid).next()
}

unsafe fn connect(argc: usize, argv: *const *const u8) -> i32 {
    let mut c = NcConnect::new();
    let mut ifarg: Option<&[u8]> = None;
    let mut pos = 0;
    let mut akm_set = None;
    let mut i = 2;
    while i < argc {
        let a = arg(argv, i);
        match a {
            b"--if" if i + 1 < argc => { ifarg = Some(arg(argv, i + 1)); i += 1; }
            b"--bssid" if i + 1 < argc => match parse_mac(arg(argv, i + 1)) {
                Some(m) => { c.bssid = m; c.flags |= connect_flags::BSSID; i += 1; }
                None => return usage(),
            },
            b"--uuid" if i + 1 < argc => match util::uuid_from_str(arg(argv, i + 1)) {
                Some(u) => { c.profile_uuid = u; i += 1; }
                None => return usage(),
            },
            b"--akm" if i + 1 < argc => match parse_akm(arg(argv, i + 1)) {
                Some(k) => { akm_set = Some(k); i += 1; }
                None => return usage(),
            },
            b"--hidden" => c.flags |= connect_flags::HIDDEN,
            _ if pos == 0 => {
                if a.is_empty() || a.len() > SSID_MAX { return usage(); }
                c.ssid[..a.len()].copy_from_slice(a);
                c.ssid_len = a.len() as u8;
                c.flags |= connect_flags::INLINE;
                pos = 1;
            }
            _ if pos == 1 => {
                if a.len() > SECRET_MAX { return usage(); }
                c.secret[..a.len()].copy_from_slice(a);
                c.secret_len = a.len() as u16;
                pos = 2;
            }
            _ => return usage(),
        }
        i += 1;
    }
    if c.flags & connect_flags::INLINE == 0 && util::uuid_is_nil(&c.profile_uuid) { return usage(); }
    if c.flags & connect_flags::INLINE != 0 && c.secret_len == 0 && akm_set.is_none() {
        // `connect SSID [--bssid MAC]` with no secret and no --akm names a saved
        // profile: resolve it like --uuid (BSSID only pins the AP).
        if let Some(u) = saved_profile_for(&c.ssid[..c.ssid_len as usize]) {
            c.profile_uuid = u;
            c.flags &= !connect_flags::INLINE;
            c.ssid = [0; SSID_MAX];
            c.ssid_len = 0;
        }
    }
    if c.flags & connect_flags::INLINE != 0 {
        c.akm = akm_set.unwrap_or(if c.secret_len == 0 { akm::NONE } else { akm::AUTO });
    }
    let n = match open_rw() { Ok(n) => n, Err(e) => { c.wipe_secret(); return fail("open /dev/netctl for writing", e) } };
    if let Some(a) = ifarg {
        match n.resolve_if(a) { Ok(ix) => c.ifindex = ix, Err(e) => { c.wipe_secret(); return fail("interface", e) } }
    }
    let r = n.ioctl(NETCTL_CONNECT, &mut c);
    c.wipe_secret();
    if let Err(e) = r { return fail("connect", e); }
    if c.attempt == 0 { sayln!("ok"); return 0; }
    sayln!("connect accepted, attempt {}", c.attempt);
    // Follow this attempt until CONNECTED or FAILED (30 s).
    let mut r = Reassembler::new();
    let mut buf = [0u8; 64 * 32];
    let deadline = now_ms() + 30_000;
    let mut live = false;
    let mut rc = -1;
    while rc < 0 && now_ms() < deadline {
        match poll1(n.fd, 500) { Ok(0) => continue, Ok(_) => {}, Err(e) => return fail("poll", e) }
        let k = match n.read_records(&mut buf) { Ok(k) => k, Err(EAGAIN) => continue, Err(e) => return fail("read", e) };
        let attempt = c.attempt;
        for_events(&mut r, &buf, k, |m, e| match e {
            Event::SnapshotEnd(_) => live = true,
            Event::WifiState(w) if live && !m.is_snapshot() && w.attempt == attempt => {
                print_wifi(&w);
                if w.state == wifi_state::CONNECTED { rc = 0; }
                if w.state == wifi_state::FAILED { rc = 1; }
            }
            _ => {}
        });
    }
    if rc < 0 { eprintln!("netctl: no outcome within 30 s"); 1 } else { rc }
}

unsafe fn simple_if_cmd(argc: usize, argv: *const *const u8, radio: Option<bool>) -> i32 {
    let mut ifarg: Option<&[u8]> = None;
    let mut keep = false;
    let first = if radio.is_some() { 3 } else { 2 };
    for i in first..argc {
        match arg(argv, i) {
            b"--keep-auto" if radio.is_none() => keep = true,
            a if ifarg.is_none() => ifarg = Some(a),
            _ => return usage(),
        }
    }
    let n = match open_rw() { Ok(n) => n, Err(e) => return fail("open /dev/netctl for writing", e) };
    let ix = match ifarg { Some(a) => match n.resolve_if(a) { Ok(i) => i, Err(e) => return fail("interface", e) }, None => 0 };
    let r = match radio {
        Some(on) => { let mut q = NcRadio::new(); q.ifindex = ix; q.on = on as u32; n.ioctl(NETCTL_SET_RADIO, &mut q) }
        None => {
            let mut q = NcIfReq::new();
            q.ifindex = ix;
            if keep { q.flags |= disconnect_flags::KEEP_AUTOCONNECT; }
            n.ioctl(NETCTL_DISCONNECT, &mut q)
        }
    };
    match r { Ok(_) => { sayln!("ok"); 0 } Err(e) => fail(if radio.is_some() { "radio" } else { "disconnect" }, e) }
}

fn print_reload(r: &NcReload) {
    let st = match r.status { profiles_status::OK => "ok", profiles_status::NO_FILE => "no saved profiles",
                              profiles_status::CORRUPT => "slots corrupt", profiles_status::IO_ERROR => "I/O error", _ => "?" };
    let src = match r.source { profiles_source::SLOT_A => "slot a", profiles_source::SLOT_B => "slot b", _ => "none" };
    sayln!("profiles generation {}: {} loaded from {} ({})", r.gen, r.count, src, st);
}

fn reload() -> i32 {
    let n = match open_rw() { Ok(n) => n, Err(e) => return fail("open /dev/netctl for writing", e) };
    match n.reload() { Ok(r) => { print_reload(&r); if r.status == profiles_status::IO_ERROR { 1 } else { 0 } } Err(e) => fail("reload", e) }
}

fn profiles() -> i32 {
    let n = match open_ro() { Ok(n) => n, Err(e) => return fail("open /dev/netctl", e) };
    let mut p = alloc::vec![NcProfileMeta::default(); MAX_PROFILES];
    match n.query(NETCTL_GET_PROFILES, 0, &mut p[..]) {
        Ok((k, count, gen)) => {
            sayln!("{} profiles (generation {})", count, gen);
            for m in &p[..k] {
                say!("  {} \"{}\" akm {} prio {}{}", Uuid(m.uuid), Esc(m.ssid()), akm_name(m.akm), m.priority,
                     if m.autoconnect != 0 { " autoconnect" } else { "" });
                for (b, s) in [(profile_flags::HIDDEN, " hidden"), (profile_flags::HAS_SECRET, " secret"),
                               (profile_flags::LEGACY, " legacy"), (profile_flags::BLOCKED_WRONG_KEY, " blocked-wrong-key")] {
                    if m.flags & b != 0 { say!("{}", s); }
                }
                sayln!("");
            }
            0
        }
        Err(e) => fail("profiles", e),
    }
}

unsafe fn save(argc: usize, argv: *const *const u8) -> i32 {
    let mut ssid: Option<&[u8]> = None;
    let mut pass: &[u8] = &[];
    let mut prio = 0i32;
    let mut hidden = false;
    let mut auto = true;
    let mut i = 2;
    while i < argc {
        let a = arg(argv, i);
        match a {
            b"--prio" if i + 1 < argc => {
                let s = arg(argv, i + 1);
                let (neg, d) = if s.first() == Some(&b'-') { (true, &s[1..]) } else { (false, s) };
                let mut v = 0i32;
                for c in d { if !c.is_ascii_digit() { return usage(); } v = v * 10 + (c - b'0') as i32; }
                prio = if neg { -v } else { v };
                i += 1;
            }
            b"--hidden" => hidden = true,
            b"--no-auto" => auto = false,
            _ if ssid.is_none() => ssid = Some(a),
            _ if pass.is_empty() => pass = a,
            _ => return usage(),
        }
        i += 1;
    }
    let ssid = match ssid { Some(s) => s, None => return usage() };
    let a = if pass.is_empty() { akm::NONE } else { akm::AUTO };
    let n = match open_rw() { Ok(n) => n, Err(e) => return fail("open /dev/netctl for writing", e) };
    let r = with_current(|list, gflags| {
        // Same SSID: update in place, keeping the uuid (as the NM shim does).
        let uuid = list.iter().find(|p| p.ssid() == ssid).map(|p| p.uuid).unwrap_or_else(new_uuid);
        list.retain(|p| p.ssid() != ssid);
        let mut p = match Profile::new_wifi(uuid, ssid, a, pass) { Some(p) => p, None => return Err(EINVAL) };
        p.priority = prio;
        p.autoconnect = auto;
        if hidden { p.flags |= profile_flags::HIDDEN; }
        list.push(p);
        write_profiles(list, gflags).map(|_| uuid)
    });
    let uuid = match r { Ok(Ok(u)) => u, Ok(Err(e)) | Err(e) => return fail("write profiles", e) };
    sayln!("saved {} \"{}\"", Uuid(uuid), Esc(ssid));
    match n.reload() { Ok(r) => { print_reload(&r); 0 } Err(e) => fail("reload", e) }
}

unsafe fn forget(argc: usize, argv: *const *const u8) -> i32 {
    if argc != 3 { return usage(); }
    let key = arg(argv, 2);
    let uuid = util::uuid_from_str(key);
    let n = match open_rw() { Ok(n) => n, Err(e) => return fail("open /dev/netctl for writing", e) };
    let r = with_current(|list, gflags| {
        let before = list.len();
        list.retain(|p| !(Some(p.uuid) == uuid || p.ssid() == key));
        if list.len() == before { return Err(ENOENT); }
        write_profiles(list, gflags).map(|_| ())
    });
    match r { Ok(Ok(())) => {} Ok(Err(e)) | Err(e) => return fail("forget", e) }
    match n.reload() { Ok(r) => { print_reload(&r); 0 } Err(e) => fail("reload", e) }
}

fn print_event(m: &EventMeta, e: &Event) {
    say!("[{:>8}] #{} {}{}", m.time_ms, m.seq, kind_ev_name(m.kind), if m.is_snapshot() { " (snapshot)" } else { "" });
    match e {
        Event::SnapshotBegin(i) | Event::SnapshotEnd(i) =>
            sayln!(" reason {} ifaces {} bss {} profiles {} writable {}", i.reason, i.n_ifaces, i.n_bss, i.n_profiles, i.writable),
        Event::Resync(r) => sayln!(" dropped {}", r.dropped),
        Event::IfaceAdded(i) | Event::IfaceChanged(i) | Event::IfaceRemoved(i) => { say!(" "); print_iface(i); }
        Event::Ipv4(v) => { say!(" if {}", v.ifindex); print_ipv4(v); }
        Event::WifiState(w) => { say!(" if {}", w.ifindex); print_wifi(w); }
        Event::WifiSignal(s) => sayln!(" if {} {}dBm {}%", s.ifindex, s.signal_dbm, s.quality),
        Event::ScanStarted(s) | Event::ScanDone(s) => sayln!(" if {} n_bss {} gen {} status {}", s.ifindex, s.n_bss, s.scan_gen, s.status),
        Event::Bss(b) | Event::BssGone(b) => { sayln!(""); print_bss(b); }
        Event::Profile(p) => sayln!(" {} \"{}\" flags 0x{:x}", Uuid(p.uuid), Esc(p.ssid()), p.flags),
        Event::ProfilesEnd(p) => sayln!(" gen {} count {} source {} status {}", p.gen, p.count, p.source, p.status),
        Event::Unknown { kind } => sayln!(" kind 0x{:x}", kind),
    }
}

fn watch(show_snapshot: bool, secs: u64) -> i32 {
    let n = match open_ro() { Ok(n) => n, Err(e) => return fail("open /dev/netctl", e) };
    let mut r = Reassembler::new();
    let mut buf = [0u8; 64 * 32];
    let deadline = if secs == 0 { u64::MAX } else { now_ms() + secs * 1000 };
    loop {
        if secs != 0 {
            let left = deadline.saturating_sub(now_ms());
            if left == 0 { return 0; }
            match poll1(n.fd, left.min(1000) as i64) { Ok(0) => continue, Ok(_) => {} Err(e) => return fail("poll", e) }
        }
        let k = match n.read_records(&mut buf) { Ok(k) => k, Err(e) => return fail("read", e) };
        for_events(&mut r, &buf, k, |m, e| if show_snapshot || !m.is_snapshot() { print_event(&m, &e) });
    }
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let argc = argc as usize;
    let cmd: &[u8] = if argc >= 2 { arg(argv, 1) } else { b"status" };
    let a2 = if argc >= 3 { Some(arg(argv, 2)) } else { None };
    match cmd {
        b"status" => status(false),
        b"ifaces" => status(true),
        b"scan" => scan(a2),
        b"bss" => {
            let n = match open_ro() { Ok(n) => n, Err(e) => return fail("open /dev/netctl", e) };
            let ix = match a2 { Some(a) => match n.resolve_if(a) { Ok(i) => i, Err(e) => return fail("interface", e) }, None => 0 };
            list_bss(&n, ix)
        }
        b"connect" => connect(argc, argv),
        b"disconnect" => simple_if_cmd(argc, argv, None),
        b"up" => match a2 {
            Some(a) if argc == 3 => {
                let n = match open_rw() { Ok(n) => n, Err(e) => return fail("open /dev/netctl for writing", e) };
                let mut c = NcConnect::new();
                c.ifindex = match n.resolve_if(a) { Ok(i) => i, Err(e) => return fail("interface", e) };
                match n.ioctl(NETCTL_CONNECT, &mut c) {
                    Ok(_) if c.attempt == 0 => { sayln!("ok"); 0 }
                    Ok(_) => { sayln!("connect accepted, attempt {} (a Wi-Fi interface: use connect)", c.attempt); 0 }
                    Err(e) => fail("up", e),
                }
            }
            _ => usage(),
        },
        b"radio" => match a2 {
            Some(b"on") => simple_if_cmd(argc, argv, Some(true)),
            Some(b"off") => simple_if_cmd(argc, argv, Some(false)),
            _ => usage(),
        },
        b"reload" => reload(),
        b"profiles" => profiles(),
        b"save" => save(argc, argv),
        b"forget" => forget(argc, argv),
        b"watch" => {
            let mut snap = true;
            let mut secs = 0u64;
            let mut i = 2;
            while i < argc {
                match arg(argv, i) {
                    b"--no-snapshot" => snap = false,
                    b"--for" if i + 1 < argc => {
                        for c in arg(argv, i + 1) { if !c.is_ascii_digit() { return usage(); } secs = secs * 10 + (c - b'0') as u64; }
                        i += 1;
                    }
                    _ => return usage(),
                }
                i += 1;
            }
            watch(snap, secs)
        }
        b"-h" | b"--help" | b"help" => { usage(); 0 }
        _ => usage(),
    }
}
