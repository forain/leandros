# lane/rtnetlink 2026-10-29 - FIXED (QEMU-verified; real Meet call pending)

Root cause (meetdiag): no AF_NETLINK, so Firefox nICEr got 0 local addresses and 0 ICE candidates.

Added
- servers/net/src/rtnetlink.rs: pure encoding. RTM_GETLINK dump (ifinfomsg + IFNAME/ADDRESS/BROADCAST/MTU/OPERSTATE),
  RTM_GETADDR dump (IPv4: ADDRESS/LOCAL/LABEL/BROADCAST), empty RTM_GETROUTE dump, NLMSG_DONE as its own datagram,
  NLMSG_ERROR(EOPNOTSUPP) for other requests. Datagrams <= 3.5 KiB. SIOCGIF* ifreq logic. 5 host unit tests:
  `rustc --edition 2021 --test servers/net/src/rtnetlink.rs && ./rtnetlink`.
- servers/net/src/nlsock.rs: socket plumbing (queue per open file description; bind, getsockname, send/sendto/sendmsg,
  recv/recvfrom/recvmsg with MSG_PEEK and MSG_TRUNC, poll). Hooks at the top of the matching handle_* in lib.rs.
  socket(AF_NETLINK, SOCK_RAW|SOCK_DGRAM, NETLINK_ROUTE); other protocols EPROTONOSUPPORT.
- kernel/src/syscall.rs: SIOCGIFNAME/CONF/FLAGS/ADDR/BRDADDR/NETMASK/MTU/HWADDR/INDEX on socket fds; SIOCETHTOOL and
  SIOCGIWRATE -> EOPNOTSUPP; unknown name -> ENODEV.
- userland/netlinktest (C, musl): 36 checks incl. getifaddrs, if_nameindex, SIOCGIFCONF; in the regress list.

Evidence: netlinktest 36/0 on aarch64 and x86_64; stunprobe in QEMU now lists lo 127.0.0.1 and eth0; nettest x86_64 21/0.

Not done / open
- No IPv6 (stack has none); no multicast groups / RTM_NEWADDR notifications; no RTM_GETROUTE content; no getsockopt on netlink.
- A blocking recv on a netlink socket with nothing queued returns EAGAIN (clients always send first).
- wifi0 has an address only when configured; lo is synthesized (index 1), netifs use index id+2.
