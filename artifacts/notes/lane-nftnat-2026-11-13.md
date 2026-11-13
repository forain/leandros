# lane/nftnat (podnat L2, datapath) 2026-11-13: PARTIAL (VM + inbound DNAT + hairpin + host-local OUTPUT)

- servers/net/src/nfnl.rs (appended): register VM over exactly the netavark capture expressions (payload, meta l4proto/oifname/mark load+set,
  ct state (always NEW), fib daddr type local, cmp eq/neq/lt/gt, bitwise, range, anonymous-set lookup, immediate verdict/jump/goto/return,
  nat dnat addr+port, masq). `eval(ns, hook, &mut Pkt) -> (accept, Nat)` runs every base chain at a hook by priority; `dnat_ports(ns)`.
  Raw stored exprs are interpreted per new flow (no compile step).
- fwd.rs: Ct.kind (0 SNAT, 1 inbound DNAT, 2 hairpin), per-bridge IP->MAC/port learn table fed in `divert` (IPv4 + ARP), `new_in` (nic_rx:
  PREROUTING DNAT, FORWARD must accept, entry cap 512), `reply_in` (container reply -> nicip:H out the NIC), `hair` (container -> local address:H
  with SETMARK mark => POSTROUTING masq to the egress bridge address + NAT port, both directions), SNAT allocator skips DNAT-claimed ports,
  learn miss => ARP request flooded on the bridge, first SYN dropped (peer retransmit finds the MAC). `emit_nic` factored out of `ingress`.
- lib.rs: `nft_output_dnat` in handle_connect (ns 0, dst host-local, OUTPUT chain), `ORIG_PEER` side table so getpeername returns 127.0.0.1:H.
- All gated on `nfnl::has_tables(0)`; no tables => old path unchanged.
- brtest (aarch64, Mac HVF): nftnat_local_redirect, nftnat_hairpin, nftnat_cross_netns, loading a netavark-shaped ruleset with real `nft -f`;
  all green (brtest fails=0). Fail-first note: before the code the three cases cannot pass (no DNAT existed; the first run with a bad nft
  ruleset showed all three red), but a clean red run on the pre-change kernel was NOT recorded.
- NOT done / open: UDP-bound 127.0.0.1 and unconnected-UDP sendto redirect (lib.rs handle_sendmsg), port-range / ct-invalid / --internal brtest
  cases, nic_rx inbound path has no in-guest test (needs a QEMU hostfwd, L3's LEANDROS_HOSTFWD), unmarked cross-network DNAT without masq
  (falls through to legacy behaviour), "no masquerade => drop" gate (§3e), ct entry TTL for Hair when A==B bridge, Pend frame parking
  (currently drop + ARP + retransmit), old nftables.rs engine untouched.
