# lane/tundev2 (podnet tundev) - 2026-11-10

/dev/net/tun implemented per design-podnet.md section 4 (replaces the abandoned lane/tundev, which built its own per-netns stacks).

- servers/net/src/tundev.rs: the fd side. Device node (10,200), the opener's netns pinned at open() (NsRef), reader queue (cap 1000), PI header, ifreq parsing, `%d` templates, TUNSETIFF (IFF_TUN|IFF_TAP|IFF_NO_PI; VNET_HDR/MULTI_QUEUE -> EINVAL), TUNGETIFF, TUNGETFEATURES, TUNSETPERSIST/OWNER/GROUP, FIONREAD, poll/epoll, O_NONBLOCK. CAP_NET_ADMIN is checked over the netns owner userns. Interface side is the `TunBackend` hook table (attach/input/detach; detach has a `keep` flag for persistence); no backend -> TUNSETIFF = ENODEV.
- servers/net/src/tunbe.rs: net's backend. attach creates a Tap/Tun vif in the opener's netns through vifcore's tables (VIFS/NETNS) and its own smoltcp stack on a dynamic sid. TapDev collects tx frames during poll_stack; they go to tundev::deliver after the stack lock is dropped. input pushes to the vif rxq (leaf lock) and kicks the daemon. IFF_TUN runs an Ethernet stack and strips/adds L2 at the fd boundary, answering ARP for the peer (no medium-ip feature needed).
- SIOCSIFFLAGS/ADDR/NETMASK/MTU on a tun/tap name in the caller's netns (kernel/src/syscall.rs -> tunbe::sock_set); netlink and SIOCGIF* dumps list tun/tap via rtnetlink::current_links.
- lib.rs: udp_stack routes a non-initial netns over its own vifs only (never the host NICs); the UDP backstop/send gates are relaxed only for AF_INET datagram sockets of a netns that has vifs. TCP and others stay gated. iface::can_route handles dynamic sids (it indexed IFRT by sid-1 and panicked under the lock).
- Test: userland/tuntest (registered in build-userland.sh, mkfs-f2fs-populated.py, runtests regress list).

Open: RTM_NEWLINK/NEWADDR on tap (ioctl only for now), TCP in a netns, tap routes via RTM_NEWROUTE, mac/ifindex stable only per boot. On main /bin/tuntest fails (no /dev/net/tun node).
