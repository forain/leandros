# lane rcvtimeo 2026-10-23 (L9.11) - REFUTED on main

The wifirate2 "recvfrom with SO_RCVTIMEO blocks forever" predates lane sotimeo (957ef59e, L9.1), which fixed it.
Re-checked on main ba79f128 (aarch64 HVF): no hang.
Added `so_rcvtimeo_iface` to userland/udptest/sotimeo.c: recvfrom, recvmsg and read on an empty UDP socket with
SO_RCVTIMEO 200 ms, bound to 127.0.0.1, 0.0.0.0 and the default-route interface address (eth0): EAGAIN after ~200 ms.
Existing cases already cover recv/read UDP, TCP accepted socket, AF_UNIX, accept. udptest PASS (fails=0, 21 s).
AF_NETLINK/AF_PACKET recv timeouts not tested (not exercised by the original report).
