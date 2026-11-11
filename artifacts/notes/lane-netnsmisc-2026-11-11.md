# lane/netnsmisc 2026-11-11 (FIXED)
- Fix 275c53d5: NAT ICMP error un-translation (fwd.rs icmp_error_rx, RFC 5508); RTM_SETLINK + IFLA_NET_NS_FD moves a link to another netns (rtnetlink.rs, tunbe.rs move_link).
- Tests: brtest fwd_icmperr (UDP to closed gateway port 9; slirp does emit the ICMP port-unreachable, so not vacuous) and veth_setlink_move.
- Red (fix reverted, aarch64 Mac): brtest 175/3 -- fwd_icmperr errno 9, veth_setlink_move x2. Green at tip: brtest 178/0.
- Already fixed, stale TODO struck: inet listener wake tags (20673804, listen::wake_key; guard = listentagtest.py, in regress) and connect(AF_UNSPEC) (inet_disconnect); new guard nettest connect_unspec.
- Still open: TCP over tap in netns, rootful `-p`, `--internal`.
