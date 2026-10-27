use std::collections::VecDeque;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress};

use crate::frames::{self, ME};
use crate::rxpolicy::{self, NifAction, RearmGuard};

/// A netif as the daemon sees it (brcmf: RXQ filled by the driver thread).
#[derive(Default)]
struct FakeNif {
    rxq: VecDeque<Vec<u8>>,
    mac: Option<[u8; 6]>,
    admin_down: bool,
    /// A driver whose `rx_rearm` says "pending" no matter what.
    sticky_pending: bool,
    tx: Vec<Vec<u8>>,
    taken: u64,
}

impl FakeNif {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        let f = self.rxq.pop_front()?;
        self.taken += 1;
        let n = f.len().min(buf.len());
        buf[..n].copy_from_slice(&f[..n]);
        Some(n)
    }
    fn rx_rearm(&self) -> bool { self.sticky_pending || !self.rxq.is_empty() }
}

/// `NicDevice` of servers/net: RX_BATCH frames, then one `None`.
struct Dev<'a> { nif: &'a mut FakeNif, batch: u32 }
struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut Vec<Vec<u8>>);

impl smoltcp::phy::RxToken for Rx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(mut self, f: F) -> R { f(&mut self.0) }
}
impl<'a> smoltcp::phy::TxToken for Tx<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0u8; len];
        let r = f(&mut b);
        self.0.push(b);
        r
    }
}
impl<'d> Device for Dev<'d> {
    type RxToken<'a> = Rx where Self: 'a;
    type TxToken<'a> = Tx<'a> where Self: 'a;
    fn receive(&mut self, _t: Instant) -> Option<(Rx, Tx<'_>)> {
        if self.batch >= 64 { self.batch = 0; return None; }
        let mut buf = [0u8; 2048];
        let n = self.nif.recv(&mut buf)?;
        self.batch += 1;
        Some((Rx(buf[..n].to_vec()), Tx(&mut self.nif.tx)))
    }
    fn transmit(&mut self, _t: Instant) -> Option<Tx<'_>> { Some(Tx(&mut self.nif.tx)) }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c
    }
}

struct Stack { iface: Interface, sockets: SocketSet<'static> }

fn build_stack(nif: &mut FakeNif) -> Stack {
    let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(nif.mac.unwrap())));
    cfg.random_seed = 0x5eed;
    let iface = Interface::new(cfg, &mut Dev { nif, batch: 0 }, Instant::from_millis(0));
    let mut sockets = SocketSet::new(vec![]);
    sockets.add(smoltcp::socket::dhcpv4::Socket::new());
    Stack { iface, sockets }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// The daemon before the fix: skip a netif without a stack (or admin
    /// down), restart the pass while any `rx_rearm` is true.
    Legacy,
    Fixed,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome { Parked { passes: u64 }, NeverParked }

/// Passes until the daemon parks; `NeverParked` after `limit` passes (on the
/// kernel that is an IRQ-off loop: the 1-CPU wedge).
fn until_park(p: Policy, nif: &mut FakeNif, stack: &mut Option<Stack>, guard: &mut RearmGuard, limit: u64) -> Outcome {
    let mut now = 1_000;
    for pass in 1..=limit {
        let poll = match p {
            Policy::Legacy => !nif.admin_down && stack.is_some(),
            Policy::Fixed => match rxpolicy::nif_action(stack.is_some(), nif.admin_down, stack.is_none() && nif.mac.is_some()) {
                NifAction::Poll => true,
                NifAction::InitStack => { *stack = Some(build_stack(nif)); true }
                NifAction::Discard => {
                    let mut b = [0u8; 2048];
                    let mut n = 0;
                    while n < rxpolicy::DISCARD_MAX && nif.recv(&mut b).is_some() { n += 1; }
                    false
                }
            },
        };
        if poll {
            let s = stack.as_mut().unwrap();
            s.iface.poll(Instant::from_millis(now), &mut Dev { nif, batch: 0 }, &mut s.sockets);
            now += 1;
        }
        let again = nif.rx_rearm();
        let restart = match p { Policy::Legacy => again, Policy::Fixed => guard.again(again) };
        if !restart { guard.parked(); return Outcome::Parked { passes: pass }; }
    }
    Outcome::NeverParked
}

fn wifi_after_carrier_up(mac_known: bool) -> FakeNif {
    let mut n = FakeNif { mac: mac_known.then_some(ME), ..Default::default() };
    for _ in 0..4 { n.rxq.extend(frames::ap_burst()); }
    n
}

/// The Pi 5 wedge: wifi0 had no stack (no MAC when the stacks were built),
/// so the old daemon never parked once AP frames sat in the driver's queue.
#[test]
fn legacy_daemon_never_parks_with_frames_for_a_stackless_netif() {
    let mut nif = wifi_after_carrier_up(true);
    let out = until_park(Policy::Legacy, &mut nif, &mut None, &mut RearmGuard::new(), 100_000);
    assert_eq!(out, Outcome::NeverParked);
    assert_eq!(nif.taken, 0, "no pass ever read the queue");
}

#[test]
fn fixed_daemon_builds_the_late_stack_and_ingests_ap_traffic() {
    let mut nif = wifi_after_carrier_up(true);
    let total = nif.rxq.len() as u64;
    let mut stack = None;
    let out = until_park(Policy::Fixed, &mut nif, &mut stack, &mut RearmGuard::new(), 100_000);
    assert!(matches!(out, Outcome::Parked { passes } if passes <= 2), "{:?}", out);
    assert!(stack.is_some());
    assert_eq!(nif.taken, total, "every AP frame went through smoltcp ingress");
    // The DHCP client starts: a DISCOVER (UDP 68 -> 67, broadcast) goes out.
    assert!(nif.tx.iter().any(|f| f.len() > 42 && f[..6] == [0xff; 6] && f[12..14] == [8, 0] && f[34..38] == [0, 68, 0, 67]),
            "no DHCP DISCOVER in {} transmitted frames", nif.tx.len());
}

#[test]
fn fixed_daemon_drops_frames_while_the_mac_is_unknown() {
    let mut nif = wifi_after_carrier_up(false);
    let mut stack = None;
    assert_eq!(until_park(Policy::Fixed, &mut nif, &mut stack, &mut RearmGuard::new(), 100_000), Outcome::Parked { passes: 1 });
    assert!(stack.is_none() && nif.rxq.is_empty());
    // The MAC arrives later (firmware up): next pass builds the stack.
    nif.mac = Some(ME);
    nif.rxq.extend(frames::ap_burst());
    until_park(Policy::Fixed, &mut nif, &mut stack, &mut RearmGuard::new(), 100_000);
    assert!(stack.is_some() && nif.rxq.is_empty());
}

#[test]
fn fixed_daemon_drops_frames_while_admin_down() {
    let mut nif = wifi_after_carrier_up(true);
    nif.admin_down = true;
    let mut stack = None;
    assert_eq!(until_park(Policy::Fixed, &mut nif, &mut stack, &mut RearmGuard::new(), 100_000), Outcome::Parked { passes: 1 });
    assert!(stack.is_none() && nif.rxq.is_empty());
}

#[test]
fn discard_is_bounded_per_pass() {
    let mut nif = FakeNif::default();
    for _ in 0..(rxpolicy::DISCARD_MAX * 3) { nif.rxq.push_back(frames::arp_request()); }
    let out = until_park(Policy::Fixed, &mut nif, &mut None, &mut RearmGuard::new(), 100);
    assert_eq!(out, Outcome::Parked { passes: 3 });
}

/// A driver that keeps saying "pending" can no longer pin the daemon.
#[test]
fn rearm_streak_is_capped() {
    let mut nif = FakeNif { mac: Some(ME), sticky_pending: true, ..Default::default() };
    let mut g = RearmGuard::new();
    let out = until_park(Policy::Fixed, &mut nif, &mut None, &mut g, 100_000);
    assert_eq!(out, Outcome::Parked { passes: rxpolicy::REARM_MAX as u64 + 1 });
    assert_eq!(g.streak(), 0, "parking resets the streak");
    assert_eq!(until_park(Policy::Legacy, &mut nif, &mut None, &mut g, 10_000), Outcome::NeverParked);
}

/// Each AP frame type alone, many times over, through a live stack: smoltcp
/// ingress always terminates and consumes the frame.
#[test]
fn every_ap_frame_type_is_consumed() {
    for (i, f) in frames::ap_burst().into_iter().enumerate() {
        let mut nif = FakeNif { mac: Some(ME), ..Default::default() };
        let mut stack = Some(build_stack(&mut nif));
        for _ in 0..500 { nif.rxq.push_back(f.clone()); }
        let out = until_park(Policy::Fixed, &mut nif, &mut stack, &mut RearmGuard::new(), 1_000);
        assert!(matches!(out, Outcome::Parked { .. }), "frame #{} ({} bytes): {:?}", i, f.len(), out);
        assert!(nif.rxq.is_empty() && nif.taken == 500, "frame #{}", i);
    }
}

#[test]
fn guard_basics() {
    let mut g = RearmGuard::new();
    assert!(!g.again(false));
    for _ in 0..rxpolicy::REARM_MAX { assert!(g.again(true)); }
    assert!(!g.again(true) && g.capped());
    g.parked();
    assert!(g.again(true));
    assert_eq!(rxpolicy::nif_action(true, false, false), NifAction::Poll);
    assert_eq!(rxpolicy::nif_action(true, true, true), NifAction::Discard);
    assert_eq!(rxpolicy::nif_action(false, false, true), NifAction::InitStack);
    assert_eq!(rxpolicy::nif_action(false, false, false), NifAction::Discard);
}
