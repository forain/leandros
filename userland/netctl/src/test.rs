//! netctltest — `/dev/netctl` conformance (docs/servers/netctl.md).
//!
//! Runs as ROOT (the regress suite logs in as root). Cases that need another
//! identity fork a child that drops to it with setgroups + setresgid +
//! setresuid; the parent reads the child's exit status as the verdict.
//!
//!   * the node: open-time permission model — a non-member gets EACCES on
//!     O_RDWR/O_WRONLY and a working read-only view with EPERM on every
//!     mutating ioctl; a netdev member (supplementary group or egid) and root
//!     get a write-capable open; an inherited descriptor keeps its open's
//!     capability; the profile slots are unreadable to a non-member;
//!   * the stream: snapshot on open (framing, order, `writable`), whole
//!     64-byte records, EINVAL for short reads and for write(2), EAGAIN when
//!     empty and non-blocking, POLLIN, a blocked reader woken by a delta,
//!     lossless overflow (RESYNC + snapshot, nothing half-delivered);
//!   * queries: lo + eth0, IPv4 bound with the default route, strides,
//!     ENODEV/ENOTTY/EINVAL paths;
//!   * profiles: the A/B writer protocol (as root and as a netdev member),
//!     RELOAD semantics, secret-free metadata, PROFILE batch events, a torn
//!     newer slot falling back to the older one, restore to empty slots;
//!   * commands without a Wi-Fi backend: ENODEV; wired admin per features.
//!
//! Output: "<case>: PASS|FAIL [detail]", then "--- netctltest done: N
//! failure(s)"; the exit status is the failure count.

#![no_std]
#![no_main]

extern crate alloc;
extern crate leandros_libc;

#[macro_use]
#[path = "common.rs"]
mod common;

use common::*;
use leandros_libc::{exit, fork, setgroups, setresgid, setresuid, wait4};
use netctl_abi::profiles::{self as pf, Profile};
use netctl_abi::*;

const UID_NOBODY: u32 = 65534;
const UID_USER: u32 = 1000;

static mut FAILS: u32 = 0;

fn verdict(name: &str, ok: bool, detail: core::fmt::Arguments) {
    if ok { sayln!("{}: PASS", name); } else {
        sayln!("{}: FAIL {}", name, detail);
        unsafe { FAILS += 1; }
    }
}

macro_rules! check {
    ($name:expr, $ok:expr) => { verdict($name, $ok, format_args!("")) };
    ($name:expr, $ok:expr, $($t:tt)*) => { verdict($name, $ok, format_args!($($t)*)) };
}

/// Run `f` in a child with the given identity; true when it exits 0.
/// `f` returns 0 for success or a step number for diagnosis.
fn as_user(uid: u32, gid: u32, groups: &[u32], f: impl FnOnce() -> i32) -> (bool, i32) {
    let pid = unsafe { fork() };
    if pid == 0 {
        let ok = unsafe {
            setgroups(groups.len(), groups.as_ptr()) == 0 && setresgid(gid, gid, gid) == 0 && setresuid(uid, uid, uid) == 0
        };
        exit(if ok { f() } else { 99 });
    }
    let mut st = 0i32;
    unsafe { wait4(pid, &mut st, 0, core::ptr::null_mut()); }
    let code = (st >> 8) & 0xff;
    (st == 0, code)
}

/// Drain everything currently queued; returns the decoded events.
fn drain(n: &Netctl, out: &mut alloc::vec::Vec<(EventMeta, Event)>) -> u32 {
    let mut r = Reassembler::new();
    let mut buf = [0u8; 64 * 64];
    let mut errs = 0;
    loop {
        match poll1(n.fd, 0) { Ok(rev) if rev & POLLIN != 0 => {} _ => break }
        let k = match n.read_records(&mut buf) { Ok(k) => k, Err(_) => break };
        if k == 0 { break; }
        errs += for_events(&mut r, &buf, k, |m, e| out.push((m, e)));
    }
    errs + if r.in_progress() { 1 } else { 0 }
}

// ── cases ──────────────────────────────────────────────────────────────────

fn case_version() {
    let n = match Netctl::open(O_RDONLY) { Ok(n) => n, Err(e) => return check!("version", false, "open {}", errname(e)) };
    match n.version() {
        Ok(v) => check!("version", v.abi_version == NETCTL_ABI_VERSION && v.size as usize == NcVersion::SIZE
                        && v.event_size == 64 && v.queue_records as usize == EVENT_QUEUE_RECORDS,
                        "abi {} size {} event {} q {}", v.abi_version, v.size, v.event_size, v.queue_records),
        Err(e) => check!("version", false, "{}", errname(e)),
    }
}

fn case_snapshot(flags: i32, writable: u32, name: &str) {
    let n = match Netctl::open(flags | O_NONBLOCK) { Ok(n) => n, Err(e) => return check!(name, false, "open {}", errname(e)) };
    let mut ev = alloc::vec::Vec::new();
    let errs = drain(&n, &mut ev);
    let first_ok = matches!(ev.first(), Some((m, Event::SnapshotBegin(i))) if m.seq == 1 && m.is_snapshot()
                            && i.writable == writable && i.reason == snapshot_reason::OPEN && i.abi_version == NETCTL_ABI_VERSION);
    let last_ok = matches!(ev.last(), Some((m, Event::SnapshotEnd(_))) if m.is_snapshot());
    let all_snap = ev.iter().all(|(m, _)| m.is_snapshot());
    let seq_ok = ev.iter().enumerate().all(|(i, (m, _))| m.seq == i as u32 + 1);
    let lo = ev.iter().any(|(_, e)| matches!(e, Event::IfaceAdded(i) if i.ifindex == IFINDEX_LO && i.kind == if_kind::LOOPBACK));
    let eth = ev.iter().any(|(_, e)| matches!(e, Event::IfaceAdded(i) if i.kind == if_kind::ETHERNET && name_of(i) == "eth0"));
    let pend = ev.iter().any(|(_, e)| matches!(e, Event::ProfilesEnd(_)));
    // Order: ifaces before ipv4 before profiles.
    let pos = |k: u8| ev.iter().position(|(m, _)| m.kind == k).unwrap_or(usize::MAX);
    let order = pos(event::kind::IFACE_ADDED) < pos(event::kind::IPV4) && pos(event::kind::IPV4) < pos(event::kind::PROFILES_END);
    check!(name, errs == 0 && first_ok && last_ok && all_snap && seq_ok && lo && eth && pend && order,
           "errs {} first {} last {} snap {} seq {} lo {} eth {} pend {} order {} n {}",
           errs, first_ok, last_ok, all_snap, seq_ok, lo, eth, pend, order, ev.len());
}

fn case_queries() {
    let n = match Netctl::open(O_RDONLY) { Ok(n) => n, Err(e) => return check!("query_ifaces", false, "open {}", errname(e)) };
    let mut l = [NcIface::default(); MAX_IFACES];
    match n.query(NETCTL_GET_IFACES, 0, &mut l) {
        Ok((k, count, _)) => {
            let ok = k as u32 == count && k >= 2 && l[0].ifindex == IFINDEX_LO && name_of(&l[0]) == "lo"
                && l[..k].iter().any(|i| i.kind == if_kind::ETHERNET && i.mac != [0; 6] && i.flags & iface_flags::ADMIN_UP != 0);
            check!("query_ifaces", ok, "k {} count {}", k, count);
        }
        Err(e) => check!("query_ifaces", false, "{}", errname(e)),
    }
    // eth0 bound, carrying the default route (wait for DHCP).
    let eth = l.iter().find(|i| i.kind == if_kind::ETHERNET).map(|i| i.ifindex).unwrap_or(2);
    let deadline = now_ms() + 20_000;
    let mut v = [NcIpv4::default(); 1];
    let mut ok = false;
    while now_ms() < deadline {
        if let Ok((1, _, _)) = n.query(NETCTL_GET_IPV4, eth, &mut v) {
            if v[0].state == ip_state::BOUND && v[0].flags & ip_flags::DEFAULT_ROUTE != 0 && v[0].flags & ip_flags::HAS_GW != 0 { ok = true; break; }
        }
        sleep_ms(250);
    }
    check!("query_ipv4_bound", ok, "state {} flags {:#x} addr {}", v[0].state, v[0].flags, Ip(v[0].addr));

    // Stride larger than ours: zero-padded; smaller than ABI-1: EINVAL;
    // cap below count: cap written, count reports all.
    let mut big = [0u8; 128 * 4];
    let mut q = NcQuery::new();
    q.elem_size = 128;
    q.cap = 4;
    q.buf = big.as_mut_ptr() as u64;
    let r = n.ioctl(NETCTL_GET_IFACES, &mut q);
    let padded = big[96..128].iter().all(|&b| b == 0) && big[128..132] == 2u32.to_le_bytes();
    check!("query_stride", r.is_ok() && q.elem_size == 96 && padded, "r {:?} elem {} padded {}", r, q.elem_size, padded);
    let mut q = NcQuery::new();
    q.elem_size = 40;
    q.cap = 1;
    q.buf = big.as_mut_ptr() as u64;
    check!("query_small_stride", n.ioctl(NETCTL_GET_IFACES, &mut q) == Err(EINVAL));
    let mut one = [NcIface::default(); 1];
    let r = n.query(NETCTL_GET_IFACES, 0, &mut one);
    check!("query_cap", matches!(r, Ok((1, c, _)) if c >= 2), "{:?}", r);
    check!("query_enodev", n.query(NETCTL_GET_IFACES, 99, &mut one).err() == Some(ENODEV));
    let mut q = NcQuery::new();
    q.size = 8;
    check!("query_short_size", n.ioctl(NETCTL_GET_IFACES, &mut q) == Err(EINVAL));
}

fn mutating_all(n: &Netctl) -> [Result<usize, i32>; 5] {
    let mut s = NcScan::new();
    let mut c = NcConnect::new();
    c.flags = connect_flags::INLINE;
    c.ssid_len = 1;
    c.ssid[0] = b'x';
    c.akm = akm::NONE;
    let mut d = NcIfReq::new();
    let mut r = NcRadio::new();
    r.on = 1;
    let mut l = NcReload::new();
    [n.ioctl(NETCTL_SCAN, &mut s), n.ioctl(NETCTL_CONNECT, &mut c), n.ioctl(NETCTL_DISCONNECT, &mut d),
     n.ioctl(NETCTL_SET_RADIO, &mut r), n.ioctl(NETCTL_RELOAD_PROFILES, &mut l)]
}

fn case_ro_eperm() {
    let n = match Netctl::open(O_RDONLY) { Ok(n) => n, Err(e) => return check!("ro_mutating_eperm", false, "open {}", errname(e)) };
    let r = mutating_all(&n);
    check!("ro_mutating_eperm", r.iter().all(|x| *x == Err(EPERM)), "{:?}", r);
    let mut v = NcVersion::new();
    check!("ro_reads_ok", n.ioctl(NETCTL_GET_VERSION, &mut v).is_ok() && n.ioctl(NETCTL_REQUEST_SNAPSHOT, &mut v).is_ok());
}

fn case_misc_errors() {
    let n = match Netctl::open(O_RDWR | O_NONBLOCK) { Ok(n) => n, Err(e) => return check!("misc", false, "open {}", errname(e)) };
    let mut ev = alloc::vec::Vec::new();
    drain(&n, &mut ev);
    let mut b = [0u8; 64];
    check!("read_empty_eagain", sys_read(n.fd, &mut b) == Err(EAGAIN));
    check!("poll_empty", poll1(n.fd, 0) == Ok(0));
    check!("read_short_einval", sys_read(n.fd, &mut b[..32]) == Err(EINVAL));
    check!("write_einval", sys_write(n.fd, b"scan\n") == Err(EINVAL));
    let mut z = [0u8; 64];
    check!("ioctl_unknown_nr", sys_ioctl(n.fd, ioc(IOC_NONE, 0x1F, 0), z.as_mut_ptr() as usize) == Err(ENOTTY));
    check!("ioctl_unknown_mut_nr", sys_ioctl(n.fd, ioc(IOC_NONE, 0x3F, 0), z.as_mut_ptr() as usize) == Err(ENOTTY));
    check!("ioctl_tcgets", sys_ioctl(n.fd, 0x5401, z.as_mut_ptr() as usize) == Err(ENOTTY));
    check!("ioctl_efault", sys_ioctl(n.fd, NETCTL_GET_IFACES, 0) == Err(14));

    // Without a Wi-Fi backend, Wi-Fi commands have no target.
    let v = n.version().unwrap_or_default();
    if v.features & features::WIFI == 0 {
        let mut s = NcScan::new();
        check!("scan_no_wifi_enodev", n.ioctl(NETCTL_SCAN, &mut s) == Err(ENODEV));
        let mut r = NcRadio::new();
        check!("radio_no_wifi_enodev", n.ioctl(NETCTL_SET_RADIO, &mut r) == Err(ENODEV));
        let mut c = NcConnect::new();
        c.flags = connect_flags::INLINE;
        check!("connect_no_wifi_enodev", n.ioctl(NETCTL_CONNECT, &mut c) == Err(ENODEV));
    } else {
        sayln!("scan_no_wifi_enodev: SKIP (a Wi-Fi backend is registered)");
    }
    let mut c = NcConnect::new();
    c.ifindex = 99;
    check!("connect_bad_if_enodev", n.ioctl(NETCTL_CONNECT, &mut c) == Err(ENODEV));
    let mut c = NcConnect::new();
    c.size = 16;
    check!("connect_short_einval", n.ioctl(NETCTL_CONNECT, &mut c) == Err(EINVAL));
    // Wired admin: CONNECT (up) always accepted; DISCONNECT per feature bit.
    let mut l = [NcIface::default(); MAX_IFACES];
    let k = n.ifaces(&mut l).unwrap_or(0);
    if let Some(eth) = l[..k].iter().find(|i| i.kind == if_kind::ETHERNET) {
        let mut c = NcConnect::new();
        c.ifindex = eth.ifindex;
        let r = n.ioctl(NETCTL_CONNECT, &mut c);
        check!("wired_connect_ok", r.is_ok() && c.attempt == 0, "{:?}", r);
        if v.features & features::WIRED_ADMIN == 0 {
            let mut d = NcIfReq::new();
            d.ifindex = eth.ifindex;
            check!("wired_disconnect_unsupported", n.ioctl(NETCTL_DISCONNECT, &mut d) == Err(EOPNOTSUPP));
        }
    }
}

/// Wired admin down/up (features::WIRED_ADMIN): DISCONNECT on eth0 clears
/// ADMIN_UP and the address at once (IFACE_CHANGED + IPV4 none), a second
/// DISCONNECT is a no-op success, CONNECT brings DHCP back to BOUND.
fn case_wired_admin() {
    let n = match Netctl::open(O_RDWR | O_NONBLOCK) { Ok(n) => n, Err(e) => return check!("wired_admin", false, "open {}", errname(e)) };
    let v = match n.version() { Ok(v) => v, Err(e) => return check!("wired_admin", false, "version {}", errname(e)) };
    if v.features & features::WIRED_ADMIN == 0 {
        sayln!("wired_admin: SKIP (features lack WIRED_ADMIN)");
        return;
    }
    let mut l = [NcIface::default(); MAX_IFACES];
    let k = n.ifaces(&mut l).unwrap_or(0);
    let Some(eth) = l[..k].iter().find(|i| i.kind == if_kind::ETHERNET).copied() else {
        sayln!("wired_admin: SKIP (no wired interface)");
        return;
    };
    let ix = eth.ifindex;
    let mut ev = alloc::vec::Vec::new();
    drain(&n, &mut ev);
    ev.clear();
    // Down.
    let t0 = now_ms();
    let mut d = NcIfReq::new();
    d.ifindex = ix;
    let r = n.ioctl(NETCTL_DISCONNECT, &mut d);
    let (mut admin_off, mut ip_none) = (false, false);
    while now_ms() < t0 + 3000 && !(admin_off && ip_none) {
        let _ = poll1(n.fd, 100);
        drain(&n, &mut ev);
        for (_, e) in ev.drain(..) {
            match e {
                Event::IfaceChanged(i) if i.ifindex == ix && i.flags & iface_flags::ADMIN_UP == 0 => admin_off = true,
                Event::Ipv4(a) if a.ifindex == ix && a.state == ip_state::NONE => ip_none = true,
                _ => {}
            }
        }
    }
    let down_ms = now_ms() - t0;
    check!("wired_admin_down", r.is_ok() && admin_off && ip_none,
           "{:?} admin_off {} ip_none {} after {} ms", r, admin_off, ip_none, down_ms);
    let mut d = NcIfReq::new();
    d.ifindex = ix;
    check!("wired_admin_down_again_ok", n.ioctl(NETCTL_DISCONNECT, &mut d).is_ok());
    // Up: DHCP again.
    let t1 = now_ms();
    let mut c = NcConnect::new();
    c.ifindex = ix;
    let r = n.ioctl(NETCTL_CONNECT, &mut c);
    let (mut admin_on, mut bound) = (false, false);
    while now_ms() < t1 + 20000 && !(admin_on && bound) {
        let _ = poll1(n.fd, 200);
        drain(&n, &mut ev);
        for (_, e) in ev.drain(..) {
            match e {
                Event::IfaceChanged(i) if i.ifindex == ix && i.flags & iface_flags::ADMIN_UP != 0 => admin_on = true,
                Event::Ipv4(a) if a.ifindex == ix && a.state == ip_state::BOUND => bound = true,
                _ => {}
            }
        }
    }
    check!("wired_admin_up_rebinds", r.is_ok() && c.attempt == 0 && admin_on && bound,
           "{:?} admin_on {} bound {} after {} ms", r, admin_on, bound, now_ms() - t1);
    sayln!("wired_admin: down {} ms, up+lease {} ms", down_ms, now_ms() - t1);
}

fn case_permissions() {
    // Root.
    check!("root_rdwr", Netctl::open(O_RDWR).is_ok());
    // A non-member: no write open, a working read-only view.
    let (ok, step) = as_user(UID_NOBODY, UID_NOBODY, &[], || {
        if Netctl::open(O_RDWR).err() != Some(EACCES) { return 1; }
        if Netctl::open(O_WRONLY).err() != Some(EACCES) { return 2; }
        let n = match Netctl::open(O_RDONLY | O_NONBLOCK) { Ok(n) => n, Err(_) => return 3 };
        if mutating_all(&n).iter().any(|r| *r != Err(EPERM)) { return 4; }
        let mut l = [NcIface::default(); MAX_IFACES];
        if n.ifaces(&mut l).unwrap_or(0) < 2 { return 5; }
        let mut ev = alloc::vec::Vec::new();
        drain(&n, &mut ev);
        if !matches!(ev.first(), Some((_, Event::SnapshotBegin(i))) if i.writable == 0) { return 6; }
        // The slots hold secrets: not even readable.
        if sys_open(b"/etc/leandros/network/profiles.a\0", O_RDONLY).err() != Some(EACCES) { return 7; }
        0
    });
    check!("nonmember_readonly", ok, "step {}", step);
    // The greeter-like case: a user with other groups but not netdev.
    let (ok, step) = as_user(UID_USER, UID_USER, &[29, 44, 104], || {
        if Netctl::open(O_RDWR).err() != Some(EACCES) { return 1; }
        if Netctl::open(O_RDONLY).is_err() { return 2; }
        0
    });
    check!("other_groups_eacces", ok, "step {}", step);
    // A member by supplementary group: write-capable.
    let (ok, step) = as_user(UID_USER, UID_USER, &[GID_NETDEV], || {
        let n = match Netctl::open(O_RDWR | O_NONBLOCK) { Ok(n) => n, Err(_) => return 1 };
        let mut ev = alloc::vec::Vec::new();
        drain(&n, &mut ev);
        if !matches!(ev.first(), Some((_, Event::SnapshotBegin(i))) if i.writable == 1) { return 2; }
        match n.reload() { Ok(r) if r.status != profiles_status::IO_ERROR => {} _ => return 3 }
        // ...and may read the slot files.
        if sys_open(b"/etc/leandros/network/profiles.a\0", O_RDONLY).is_err() { return 4; }
        0
    });
    check!("member_supplementary_rw", ok, "step {}", step);
    // A member by egid.
    let (ok, step) = as_user(UID_USER, GID_NETDEV, &[], || {
        let n = match Netctl::open(O_RDWR) { Ok(n) => n, Err(_) => return 1 };
        if n.reload().is_err() { return 2; }
        0
    });
    check!("member_egid_rw", ok, "step {}", step);
    // An inherited descriptor keeps the capability of its open.
    let n = match Netctl::open(O_RDWR) { Ok(n) => n, Err(_) => return check!("inherited_fd_keeps_write", false, "open") };
    let fd = n.fd;
    let (ok, step) = as_user(UID_NOBODY, UID_NOBODY, &[], || {
        let mut r = NcReload::new();
        match sys_ioctl(fd, NETCTL_RELOAD_PROFILES, &mut r as *mut NcReload as usize) { Ok(_) => 0, Err(_) => 1 }
    });
    check!("inherited_fd_keeps_write", ok, "step {}", step);
}

fn case_stream() {
    // POLLIN + a blocked reader woken by a delta (RELOAD emits a batch).
    let ro = match Netctl::open(O_RDONLY | O_NONBLOCK) { Ok(n) => n, Err(e) => return check!("stream", false, "open {}", errname(e)) };
    let rw = match Netctl::open(O_RDWR) { Ok(n) => n, Err(e) => return check!("stream", false, "open rw {}", errname(e)) };
    let mut ev = alloc::vec::Vec::new();
    drain(&ro, &mut ev);
    let r = rw.reload();
    let pollin = poll1(ro.fd, 1000).map(|x| x & POLLIN != 0).unwrap_or(false);
    ev.clear();
    drain(&ro, &mut ev);
    let gen = r.map(|r| r.gen).unwrap_or(0);
    let batch = matches!(ev.last(), Some((m, Event::ProfilesEnd(p))) if p.gen == gen && !m.is_snapshot());
    check!("pollin_on_delta", pollin && batch, "pollin {} batch {} n {}", pollin, batch, ev.len());

    // A child blocked in read(2) (blocking fd) wakes for the next batch.
    let pid = unsafe { fork() };
    if pid == 0 {
        let n = match Netctl::open(O_RDONLY) { Ok(n) => n, Err(_) => exit(1) };
        let mut r = Reassembler::new();
        let mut buf = [0u8; 64 * 16];
        let mut live = false;
        loop {
            let k = match n.read_records(&mut buf) { Ok(k) => k, Err(_) => exit(2) };
            let mut got = false;
            for_events(&mut r, &buf, k, |m, e| match e {
                Event::SnapshotEnd(_) => live = true,
                Event::ProfilesEnd(_) if live && !m.is_snapshot() => got = true,
                _ => {}
            });
            if got { exit(0); }
        }
    }
    sleep_ms(500);
    let t0 = now_ms();
    let _ = rw.reload();
    // Reap within 3 s (WNOHANG polling).
    let mut st = -1i32;
    let mut done = false;
    while now_ms() < t0 + 3000 {
        if unsafe { wait4(pid, &mut st, 1, core::ptr::null_mut()) } == pid { done = true; break; }
        sleep_ms(20);
    }
    if !done { unsafe { leandros_libc::syscall::syscall2(leandros_libc::syscall::nr::KILL, pid as usize, 9); wait4(pid, &mut st, 0, core::ptr::null_mut()); } }
    check!("blocked_reader_woken", done && st == 0, "done {} status {:#x} after {} ms", done, st, now_ms() - t0);

    // Overflow: ask for snapshots without reading until the queue cannot
    // hold another; the reader then gets RESYNC + one full snapshot.
    let mut v = NcVersion::new();
    for _ in 0..200 { let _ = ro.ioctl(NETCTL_REQUEST_SNAPSHOT, &mut v); }
    ev.clear();
    let errs = drain(&ro, &mut ev);
    let first_resync = matches!(ev.first(), Some((_, Event::Resync(r))) if r.dropped > 0);
    let begin = matches!(ev.get(1), Some((m, Event::SnapshotBegin(i))) if i.reason == snapshot_reason::RESYNC && m.is_snapshot());
    let end = matches!(ev.last(), Some((_, Event::SnapshotEnd(i))) if i.reason == snapshot_reason::RESYNC);
    let begins = ev.iter().filter(|(_, e)| matches!(e, Event::SnapshotBegin(_))).count();
    let seq_jump = match (ev.first(), ev.get(1)) { (Some((a, _)), Some((b, _))) => b.seq == a.seq + 1, _ => false };
    check!("overflow_resync", errs == 0 && first_resync && begin && end && begins == 1 && seq_jump,
           "errs {} resync {} begin {} end {} begins {} seq {} n {}", errs, first_resync, begin, end, begins, seq_jump, ev.len());
    // FIONREAD on an empty queue.
    let mut q = -1i32;
    check!("fionread", sys_ioctl(ro.fd, 0x541B, &mut q as *mut i32 as usize).is_ok() && q == 0, "{}", q);
}

fn mkprof(ssid: &[u8], secret: &[u8]) -> Profile {
    Profile::new_wifi(new_uuid(), ssid, if secret.is_empty() { akm::NONE } else { akm::AUTO }, secret).unwrap()
}

fn case_profiles() {
    let rw = match Netctl::open(O_RDWR) { Ok(n) => n, Err(e) => return check!("profiles", false, "open {}", errname(e)) };
    let watcher = match Netctl::open(O_RDONLY | O_NONBLOCK) { Ok(n) => n, Err(e) => return check!("profiles", false, "open ro {}", errname(e)) };
    let mut ev = alloc::vec::Vec::new();
    drain(&watcher, &mut ev);
    let _ = rw.reload();
    let n_legacy_before = {
        let mut p = alloc::vec![NcProfileMeta::default(); MAX_PROFILES];
        rw.query(NETCTL_GET_PROFILES, 0, &mut p[..]).map(|(k, _, _)| p[..k].iter().filter(|m| m.flags & profile_flags::LEGACY != 0).count()).unwrap_or(0)
    };

    const SECRET: &[u8] = b"netctltest-secret-1";
    let p1 = mkprof(b"NetctlTestHome", SECRET);
    let p2 = mkprof(b"NetctlTestOpen", b"");
    let (u1, u2) = (p1.uuid, p2.uuid);
    let w1 = write_profiles(&[p1.clone(), p2.clone()], 0);
    let r1 = rw.reload();
    let ok1 = matches!((&w1, &r1), (Ok(_), Ok(r)) if r.status == profiles_status::OK && r.count as usize == 2 + n_legacy_before
                       && (r.source == profiles_source::SLOT_A || r.source == profiles_source::SLOT_B));
    check!("profiles_write_reload", ok1, "write {:?} reload {:?}", w1.as_ref().map(|x| x.1), r1.as_ref().map(|r| (r.status, r.count, r.source)));

    let mut metas = alloc::vec![NcProfileMeta::default(); MAX_PROFILES];
    let q = rw.query(NETCTL_GET_PROFILES, 0, &mut metas[..]);
    let (k, gen) = match q { Ok((k, _, g)) => (k, g), Err(_) => (0, 0) };
    let m1 = metas[..k].iter().find(|m| m.uuid == u1);
    let m2 = metas[..k].iter().find(|m| m.uuid == u2);
    let flags_ok = matches!(m1, Some(m) if m.flags & profile_flags::HAS_SECRET != 0 && m.ssid() == b"NetctlTestHome" && m.akm == akm::AUTO)
        && matches!(m2, Some(m) if m.flags & profile_flags::HAS_SECRET == 0 && m.akm == akm::NONE);
    // The secret never leaves the kernel.
    let mut leak = false;
    for m in &metas[..k] {
        let b = m.as_bytes();
        if b.windows(SECRET.len()).any(|w| w == SECRET) { leak = true; }
    }
    check!("profiles_meta_no_secret", flags_ok && !leak && r1.as_ref().map(|r| r.gen == gen).unwrap_or(false),
           "flags {} leak {} gen {}", flags_ok, leak, gen);

    ev.clear();
    drain(&watcher, &mut ev);
    let profs = ev.iter().filter(|(m, e)| matches!(e, Event::Profile(p) if p.gen == gen) && !m.is_snapshot()).count();
    let end = ev.iter().any(|(_, e)| matches!(e, Event::ProfilesEnd(p) if p.gen == gen && p.status == profiles_status::OK));
    check!("profiles_batch_events", profs == 2 + n_legacy_before && end, "profiles {} end {}", profs, end);

    // A second write lands in the other slot; tear it: the older one rules.
    let p3 = mkprof(b"NetctlTestThird", b"another-secret");
    let w2 = write_profiles(&[p1.clone(), p2.clone(), p3.clone()], 0);
    let r2 = rw.reload();
    let three = matches!(&r2, Ok(r) if r.count as usize == 3 + n_legacy_before);
    if let (Ok((slot, _)), Ok((slot2, _))) = (&w1, &w2) {
        // Corrupt the newer slot (slot2) by truncating its CRC.
        let mut img = alloc::vec![0u8; pf::FILE_MAX];
        let len = pf::serialize(99, 0, &[p1.clone()], &mut img).unwrap();
        let _ = write_file(slot_path(*slot2), &img[..len - 2]);
        let r3 = rw.reload();
        let fell_back = matches!(&r3, Ok(r) if r.status == profiles_status::OK && r.count as usize == 2 + n_legacy_before
                                 && r.source == match slot { pf::Slot::A => profiles_source::SLOT_A, pf::Slot::B => profiles_source::SLOT_B });
        check!("profiles_torn_slot_fallback", three && slot != slot2 && fell_back, "three {} r3 {:?}", three,
               r3.as_ref().map(|r| (r.status, r.count, r.source)));
    } else {
        check!("profiles_torn_slot_fallback", false, "writes failed");
    }

    // A netdev member runs the writer protocol itself.
    let (ok, step) = as_user(UID_USER, UID_USER, &[GID_NETDEV], || {
        let n = match Netctl::open(O_RDWR) { Ok(n) => n, Err(_) => return 1 };
        let p = Profile::new_wifi(new_uuid(), b"NetctlTestMember", akm::AUTO, b"member-pass-1").unwrap();
        if write_profiles(&[p], 0).is_err() { return 2; }
        match n.reload() { Ok(r) if r.status == profiles_status::OK => 0, _ => 3 }
    });
    check!("profiles_member_writes", ok, "step {}", step);

    // Restore: both slots empty -> no saved profiles (legacy only).
    let e1 = write_file(slot_path(pf::Slot::A), b"");
    let e2 = write_file(slot_path(pf::Slot::B), b"");
    let r = rw.reload();
    check!("profiles_restore_empty", e1.is_ok() && e2.is_ok()
           && matches!(&r, Ok(r) if r.status == profiles_status::NO_FILE && r.count as usize == n_legacy_before),
           "{:?}", r.as_ref().map(|r| (r.status, r.count)));
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if leandros_libc::geteuid() != 0 {
        sayln!("netctltest: must run as root");
        return 1;
    }
    case_version();
    case_snapshot(O_RDONLY, 0, "snapshot_on_open_ro");
    case_snapshot(O_RDWR, 1, "snapshot_on_open_rw");
    case_queries();
    case_ro_eperm();
    case_misc_errors();
    case_permissions();
    case_stream();
    case_profiles();
    // Last: it takes eth0 down and up again (DHCP).
    case_wired_admin();
    let f = FAILS;
    sayln!("--- netctltest done: {} failure(s)", f);
    f as i32
}
