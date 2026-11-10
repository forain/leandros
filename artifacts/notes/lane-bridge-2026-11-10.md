# lane/bridge: L2 bridge, FDB and IFLA_MASTER (L2 of design-podnet3) - FIXED

## What landed
- servers/net/src/l2.rs: the switch. Per-bridge config (AGEING_TIME, FORWARD_DELAY, STP_STATE, VLAN_FILTERING),
  an FDB that learns the source MAC of frames entering on a port, ages entries out after AGEING_TIME,
  floods unknown unicast to the other forwarding ports and broadcast/multicast to those plus the bridge itself.
- vifsel/vif: `VifKind::Bridge`, `Vif.master` (a port owns no stack). `ns_sids_dyn`/`all_dyn_sids` skip ports.
  `free_vifs`/`reap` also drop FDB state.
- tunbe.rs: bridge_create (RTM_NEWLINK kind=bridge: name, MTU, IFLA_ADDRESS, INFO_DATA options),
  enslave/release (IFLA_MASTER at veth create on the main end and on VETH_INFO_PEER, and via SETLINK),
  DELLINK of a bridge releases its ports, poll_all hands port rx to the switch and bridge-stack tx to the switch.
- rtnetlink.rs: bridge INFO_DATA (FORWARD_DELAY/AGEING_TIME/STP_STATE u32, VLAN_FILTERING u8), IFLA_MASTER on
  ports, IFLA_MASTER dump filter, DELLINK for bridges.
- Permissions: unchanged Linux rule, CAP_NET_ADMIN over the owner userns of the netns being changed
  (rtnl_apply gate; the peer netns is checked in veth_create). The bridge and port must be in the same netns.
- brtest: bridge_l2, bridge_getlink, bridge_teardown.

## Evidence
- Fail-first (box aarch64, test-only commit 68f16e79): brtest RC=1, RTM_NEWLINK kind=bridge fails (bridge_l2/getlink/teardown FAIL).
- After: brtest PASS (netns A and B behind br0 ping/UDP/TCP each other, B reaches the bridge address and a host 0.0.0.0 listener, host pings A).

## Open
- VLAN filtering and STP are stored and reported only (no behaviour). Ageing is checked at lookup, not by a timer.
- The veth-lane note about a netns that ran TCP being reaped late still holds: brtest compares counts against that baseline.
