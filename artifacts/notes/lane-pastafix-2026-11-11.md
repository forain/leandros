# lane/pastafix 2026-11-11 (PARTIAL: regression fixed by guard, root cause of v6-in-netns TCP stall open)
- Regression: rootless podman port forwarding (pasta_portfwd) failed 4/4 since ip6lo b006b83f.
- Cause: ip6lo made socket(AF_INET6) succeed. pasta, in a container netns, then builds its TCP relay on AF_INET6 sockets; the relay stalls (peer FIN/reply never noticed). Pre-ip6lo AF_INET6 was EAFNOSUPPORT everywhere, so pasta stayed on AF_INET.
- Bisect (aarch64 HVF, host:podpastatest.py): reverting wildcard_family + Cargo iface limits: still fail; all AF_INET6 refused: pass; only AF_INET6 DGRAM allowed: pass; AF_INET6 STREAM refused only in netns 0: fail; => the stall is AF_INET6 TCP created inside a non-initial netns.
- Fix: socket(AF_INET6) in a non-initial netns returns EAFNOSUPPORT (no v6 in netns yet). ip6test green, pasta 4/4 (CHURN=1).
- brtest x5: 3/5 pass (fwd_tcp+fwd_port_reserved, veth_tcp_reap flaked) -> the brtest NAT flakes are NOT this cause (brtest does not use AF_INET6).
- Open: find why an AF_INET6 TCP socket in a netns stalls (readiness/wake), then drop the guard. See TODO.md.
