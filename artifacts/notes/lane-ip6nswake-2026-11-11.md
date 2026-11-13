# lane/ip6nswake 2026-11-11 (FIXED: pastafix's EAFNOSUPPORT guard removed)
- Real cause: NOT a wake/readiness bug. handle_send_k refuses every send on a domain-10 socket in a non-initial netns with ENETUNREACH (the nslo P2 "netns stacks are v4-only" backstop, eb26baa1). Since ip6lo, AF_INET6 sockets exist there; passt's v4-mapped/dual relay sockets could never send, so the relay stalled.
- Fix: the refusal no longer applies to a connected stream socket (only a v4-mapped flow can be connected there; connect refuses real v6). UDP/unconnected v6 sends in a netns stay refused. Guard in handle_socket removed.
- Test: ip6test test_netns_v6_tcp (unshare CLONE_NEWNET + lo up; peer writes then closes; client poll POLLIN, read data, read EOF): v6->dual, v4->dual, v6->v4, v6 ::1, v4 baseline. Red with the guard removed (write EPIPE-class -101 on accepted v6 socket, errno 10), green after.
- Gap noticed: net poll never reports POLLRDHUP for a TCP peer FIN (only UDP shutdown bits); not asserted. 
