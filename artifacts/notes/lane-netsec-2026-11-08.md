# lane/netsec 2026-11-08 — G1 netns escape via sendto/sendmsg sa_family (FIXED)

Bug: servers/net handle_send_k gated child-netns sends on the user-supplied
sa_family (and re-read user memory later: TOCTOU). AF_INET UDP/ICMP sockets
with sin_family=AF_UNIX/AF_INET6/0 reached udp_materialize and left via the host stack.

Fix (servers/net/src/lib.rs): gate on the socket's own domain/state and captured netns
(`sock_inet_domain_in_netns`, any inet socket incl. ICMP, no user memory). User family is
read only to choose the errno (mismatch -> EAFNOSUPPORT as Linux, else ENETUNREACH).
Backstop: `udp_materialize` returns ENETUNREACH when the slot's netns != 0.

Test: nstest `netns_sendfam` (child NEWNET; sendto + sendmsg with AF_UNIX/AF_INET6/0
families, ICMP ping socket; host listener 127.0.0.1:41047 must receive nothing).
Unfixed main: FAIL (exploit succeeds). Fixed: nstest + nettest PASS aarch64.

Audit: connect (~4778) and bind (~3992) fetch sa_family once and use it for both gate and
dispatch (no TOCTOU, no double-fetch). Residual (not exploitable for egress): an inet socket
connect() with sa_family=AF_UNIX dispatches on the unix path and fails by state.
