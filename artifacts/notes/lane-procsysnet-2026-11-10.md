# lane/procsysnet 2026-11-10 (podtrace4 blocker #2) - FIXED

/proc/sys/net exists and is per network namespace (sched::ns NETCTL table, reset in new_flat; VFS open/write intercept).
Files: core/{somaxconn,rmem_max,wmem_max}; ipv4/{ip_forward,ip_unprivileged_port_start,ip_local_port_range,ping_group_range,
ip_nonlocal_bind,ip_default_ttl,tcp_syncookies}; ipv4/conf/{all,default,lo}/{forwarding,route_localnet,rp_filter,arp_notify};
ipv6/conf/{all,default,lo}/{disable_ipv6,forwarding,accept_ra,accept_dad}.
Writes: open for write needs CAP_NET_ADMIN over the netns owner userns, else EACCES (as Linux); invalid values EINVAL.
Honoured: ping_group_range (SOCK_DGRAM IPPROTO_ICMP socket(), EACCES; CAP_NET_RAW over the netns owner also passes),
ip_unprivileged_port_start (bind; CAP_NET_BIND_SERVICE over the netns owner passes, so userns root can bind low ports in its netns).
Stored only (not yet honoured): everything else incl. somaxconn, ip_local_port_range, forwarding.
Deviation: init-ns ping_group_range defaults to "0 2147483647" (distro/systemd value, keeps unprivileged ping working);
new netns gets the kernel default "1 0".
Tests: nstest procsysnet_{defaults,netns_isolated,root_write,unpriv_denied,port_start,ping_range}. Mac aarch64: nettest, nstest, ffsandbox on=True off=False.
Not run on unfixed main (/proc/sys/net is absent there, so every case fails by construction).
