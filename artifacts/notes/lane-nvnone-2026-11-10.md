# lane/nvnone 2026-11-10 (podnet3 N0)
PARTIAL. Delivered: ports/podman/containers.conf (firewall_driver none) installed via scripts/mkfs-f2fs-populated.py; `net.nltrace=1` cmdline knob in servers/net/src/nlsock.rs (REQ type/flags/fixed/attrs one nesting level, REP errno/type).
Not delivered: the captured netavark sequence. Findings (aarch64 HVF, idmap=test):
- root podman run still fails at the /run/netns bind mount (EINVAL) before netavark runs;
- netavark run directly fails on connect(2) of AF_NETLINK: ECONNREFUSED (os error 111), so no request reaches nlsock.
Details in orch/designs/design-podnet3.md section 7 and podtrace5-findings.md "after nvnone". Guest has no unshare/pgrep, so a private netns for a direct netavark run needs the bind mount fixed first.
