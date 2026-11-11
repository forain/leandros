# lane/podports 2026-11-11 (PARTIAL: item 1 stale-struck; -p and --internal HANDED OFF with a measured design)
- TCP over a tap in a netns: already covered (lane nstcp): tuntest tcp_out/tcp_in unshare CLONE_NEWNET and run connect + listen/accept over a tap. TODO line struck.
- Measured (aarch64 Mac, root podman, firewall_driver=iptables with a logging `iptables` shell shim; needs `--version` to print "iptables v1.8.10 (legacy)"):
  netavark's whole -p programme is a line-oriented `iptables -t nat` script. The load-bearing rule, one per published port:
  `-A NETAVARK-DN-<hash> -j DNAT -p tcp --to-destination 10.88.0.2:80 --destination-port 8080` (also SETMARK rules for -s 10.88.0.0/16 and -s 127.0.0.1,
  chain jump `-A NETAVARK-HOSTPORT-DNAT -j NETAVARK-DN-<hash> -p tcp --dport 8080 -m comment ...`, `-A PREROUTING -j NETAVARK-HOSTPORT-DNAT -m addrtype --dst-type LOCAL`).
  Non-internal bridges also add `-t filter -A NETAVARK_FORWARD -s 10.88.0.0/16 -j ACCEPT`. -C / -S / -L probes: exit 1 = absent. Teardown uses -D of the same lines.
  `podman port` reports 0.0.0.0:8080 even with driver none, but nothing forwards.
- --internal: with the none driver it already behaves for the common case: container gets only the subnet route (no default), host ip_forward stays 0, wget to 10.0.2.2 = "Network unreachable".
  Residual hole: ip_forward=1 (set by any non-internal network) fans out to every bridge, so an internal container that adds its own default route is forwarded by fwd.rs divert();
  Linux/netavark blocks that with an iptables DROP, not with the kernel. Needs the same shim (no NETAVARK_FORWARD ACCEPT for the subnet => no forwarding for that bridge).
- Proposed design (not built): ship a tiny `iptables` (C or sh) that stores the nat/filter rules, and a net-server rule table fed from it
  (new write channel, e.g. a private NETLINK proto or a writable /proc/net node; procnet is read-only today).
  Kernel side in fwd.rs: inbound DNAT entry kind in Ct (host nicip:HOSTPORT -> ctr ip:port via bridge), reply path must hit the DNAT table BEFORE ingress()'s SNAT path
  (else the container's reply to the external peer opens a fresh SNAT flow), published ports must count in `reserved()`/host_port_busy so a container cannot bypass the NAT port reservation
  and the host cannot bind them; host-stack connect() to nicip:HOSTPORT / 127.0.0.1:HOSTPORT redirected at connect time (netavark's OUTPUT/localhost case).
  Then flip containers.conf to firewall_driver=iptables, add podbridgetest checks (publish -p 18081:80, nc listener in alpine, connect from host stack + Mac).
- No code changed; tests not red/green (nothing to fail yet).
