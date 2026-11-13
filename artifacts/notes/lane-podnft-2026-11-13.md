# lane/podnft (podnat L3) - 2026-11-13

DELIVERED (aarch64 Mac QEMU green; box regress both arches see below).

- `ports/podman/containers.conf`: `firewall_driver = "nftables"` (was `none`, nvnone d8b51fae).
- Kernel bug found by running it: `servers/net/src/nfnl.rs` DELCHAIN returned EBUSY for any chain that still had its own rules.
  Linux (nf_tables_delchain -> nft_delrule_by_chain) removes the chain's rules with it and only jumps from other chains
  keep it busy; netavark's teardown (capture b_teardown) sends DELCHAIN with no DELRULEs for the chain. Result before the fix:
  every `podman rm -f` of a published container failed ("removing container ... network: netavark: nftables error"), rules leaked.
  nfttest gained two cases (34/0).
- `podbridgetest.py` new checks: podman_nft_driver, publish_local (127.0.0.1 and NIC address, from the guest host netns),
  publish_ctr (bridge gw and NIC address from another container), publish_ext (Mac via QEMU hostfwd net0 -> nic_rx, or the
  vmnet address directly), publish_udp (-p 127.0.0.1:19090:90/udp), publish_range, internal_unreachable (--internal plus a
  hand-added default route, wget fails), publish_teardown (5 create/remove cycles; `nft list ruleset` md5 equals the baseline).
  `driver.py`: LEANDROS_HOSTFWD (net0 hostfwd rules); `runtests.py` sets 18581->18081 for podbridge runs.
- Results (Mac HVF aarch64): podbridgetest 20/20 PASS (233 s), podpastatest PASS (rootless pasta unchanged), nfttest 34/0.
- Fail-first: the first run on this branch (before the DELCHAIN fix) was red on publish_local_nicip/ctr_nicip (wrong test
  address, test bug), publish_ext (hostfwd env not set), publish_teardown (leaked rules) and printed netavark teardown errors on
  every `run --rm`. The `firewall_driver = "none"` baseline was not re-run: with no rules there is no DNAT, so -p cannot publish.

Open / notes
- `podman_internal_unreachable` output shows "aardvark-dns failed to start" on `network rm` of the internal net (pre-existing, not gated).
- Filter chains run on new flows only; no routed forwarding without NAT (documented in docs/userland/containers.md).
- Rootless custom bridge networks (netavark in a userns netns) not tested.
