# lane/podbridge 2026-11-10 (podnet3 L5) -- PARTIAL
Delivered: opt-in suite `--suite podbridge` (host:podbridgetest.py, implies cmdline idmap=test), `fwd: conntrack N` line in /proc/net/nic,
box/regress.sh ships /tmp/mntpivot2-img/alpine-oci[-x86_64].tar and sets LEANDROS_IMAGE_ARCHIVE (amd64 archive made with `docker save --platform linux/amd64`).
Not in the default regress list (two checks below still red).
Results (root, default network, fresh rebuilt image, a fresh image shows NO rollback/ERRO on the first run; state from killed runs does):
- x86_64 KVM (box): echo PASS, wget to host 10.0.2.2 PASS, dns PASS, 20x churn PASS, leaks PASS (no veth/podman0, no /run/netns/* mounts, conntrack 0); bridge_ip FAIL.
- aarch64 (Mac HVF gw 192.168.105.1, box TCG): echo/hostnet/dns/leaks PASS; bridge_ip FAIL; churn 1-4 of 20-40 `run --rm true` exit 127.
Open 1: bridge_ip: container -> http://10.88.0.1:18080 (httpd in a --network=host container) gives ECONNREFUSED on both arches; unverified whether httpd listens (check `cat /proc/net/listen` + which stack owns 10.88.0.1).
Open 2: RC=127 race, bridge only (--net=none 0/30, bridge 1-4/25-40): podman's events.log has create,init,start,attach,remove but NO "died"; cleanup (conmon exit cmd, incl. netavark teardown) removes the
container while the parent waits, so the parent cannot read the exit code (podman maps that to 127; sometimes also logs "Rolling back transaction").
Likely visibility of the conmon exit file / events.log between the two podman processes; not root-caused.
