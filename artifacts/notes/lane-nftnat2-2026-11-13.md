# lane/nftnat2 (podnat L2 follow-up) 2026-11-13: PARTIAL

Step 0 (fail-first against pre-nftnat net server, f492de41^1 servers/net + this lane's brtest): nftnat_local_redirect, nftnat_hairpin,
nftnat_cross_netns all RED with `nft -f` LOADING fine (PASS "nft -f loads the netavark-shaped ruleset"); the failures are the missing DNAT
(host connect 127.0.0.1:8080 never reaches the listener, server exit 11). Not a syntax error.

New brtest cases (fail-first on the f492de41 server, then green):
- nftnat_udp_local: connected UDP 127.0.0.1:9090 already worked (connect hook); unconnected sendto RED -> green (handle_send_k dgram arms run
  `nft_output_dnat`; `udp_recv_k` maps the container source back to 127.0.0.1:9090 via ORIG_PEER, so recvfrom reports 127.0.0.1:9090).
- nftnat_port_range (8000-8002, one 1:1 dnat per port): already GREEN on f492de41 (range expr + dnat_ports were right); kept as a guard.
- nftnat_internal / nftnat_masq_ok: with a nat/postrouting chain loaded, `ingress` now needs FORWARD accept + a Masq verdict for a new outbound
  flow (`nfnl::has_hook`); a subnet with no masquerade rule (netavark --internal, whose only isolation is forwarding=0, which a later
  ip_forward=1 clobbers) is no longer SNATed. RED -> green; masq_ok control green. Caveat: global once any ns-0 postrouting chain exists
  (netavark's empty skeleton stays after teardown), so non-podman bridges need their own masquerade rule while that table is loaded.
- `ct state invalid drop` is in every ruleset and the VM treats state as NEW; stray non-SYN packets never create flows, matching Linux
  (INVALID is not DNATed). No separate red case was possible.
- Learn miss: first SYN is parked (<=16, 2 s) and replayed from `divert` when the ARP reply teaches the MAC (same-bridge hairpin only).
  nftnat_cross_netns measures the first connect: still ~700 ms, so the ~700 ms is NOT (only) the dropped SYN. Reported as KNOWN-OPEN, not gated.

OPEN: first-connect latency root cause (<500 ms goal not met; suspect client-side ARP/neighbor delay for the gateway, measure per phase);
NIC-inbound (`new_in`) learn-miss parking untested/unimplemented (needs hostfwd, L3); item 3 (cross-network DNAT without mark ->
legacy `return false` in `hair`, Linux would DNAT+route unmasqueraded, subject to isolation drops) NOT done, no brtest yet.
