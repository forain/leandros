# lane/rtmetric 2026-10-30 - per-interface route metric

FIXED. 
- `netctl metric <if> <n>` / `netctl metric <if> default`: new ioctl NETCTL_SET_METRIC (0x25, NcMetric) in lib/netctl-abi; handled in servers/net/src/netctl (links::set_metric -> iface::set_metric), sets IfRt.metric, kicks netctl so the views update and IfaceChanged is emitted. Write-gated like every other mutating ioctl (read-only open gets EPERM; root and netdev group may write).
- Boot knob `net.metric.<ifname>=<n>` read in iface::default_metric (so `default` restores the knob value).
- On-link routes already use longest prefix first (route_select), so eth0's 192.168.2.0/24 stays reachable when the default route moves to wifi0; verified /proc/net/route keeps the eth0 on-link entry.
- NetworkManager shim (ports/sysbus/src/nm): no ipv4.route-metric anywhere; NOT wired (not a small change). Open.
- Existing sockets keep their stack; only new flows follow the default route.

Evidence (aarch64, wifisim NIC2): host:netmetrictest.py 4/4 (boot knob orders wifi0 before eth0, flip, restore, on-link). netctltest ro-EPERM case covers the new ioctl. aarch64 regress PASS; box x86_64 regress PASS (623 s).
