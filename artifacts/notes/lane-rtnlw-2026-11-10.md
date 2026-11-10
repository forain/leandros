# lane/rtnlw: rtnetlink writes on tun/tap links (S2 of design-podnet3) - FIXED

## What landed
- servers/net/src/rtnetlink.rs: RtOp + parse_write; RTM_NEWLINK/SETLINK (up, mtu, mac, rename), NEWADDR/DELADDR, NEWROUTE/DELROUTE
  on tun/tap links (incl. ns 0); EPERM without CAP_NET_ADMIN in the netns owner userns; IFLA_TXQLEN, LINKINFO kind "tun", IFLA_MASTER dump filter.
- servers/net/src/tunbe.rs: rtnl_apply (addr max 2, routes into NetNsState.routes + smoltcp gateway routes).
- servers/net/src/nlsock.rs: connect()/getpeername() on AF_NETLINK (nvnone request).
- userland/netlinktest: fail-first suite (tap_cfg_netlink, newroute_needs_cap, nl_connect_kernel, ...).

## The hang (root cause)
tap_cfg_netlink blocked at the first RTM_NEWADDR: the request was applied but NO reply was produced (traced: `replies n=0`).
`dump` was computed as `flags & NLM_F_DUMP != 0`. NLM_F_DUMP is ROOT|MATCH (0x300), and on NEW/DEL requests the same bits mean
REPLACE (0x100) / EXCL (0x200). A NEWADDR with `NLM_F_CREATE|NLM_F_EXCL|NLM_F_ACK` (0x605) was therefore treated as a dump, which
suppresses the ACK, so the client waited forever in recv. kick() and locks were innocent (dropping kick() did not change it).
Fix: a dump is only recognized on RTM_GET* requests (`ty & 3 == 2`), as in Linux rtnetlink_rcv_msg.

## Evidence
- Traced the reply path with temporary serial markers: apply returned, handle_request_env returned, replies n=0.
- Box aarch64 /bin/netlinktest after fix: 1/1 PASS (76 case PASS lines); before: FAIL addr_dump_has, deladdr, delroute_* (see handoff).
- Host unit tests in rtnetlink.rs cannot be built on the Mac (crate has kernel asm); covered by netlinktest on the guest.
