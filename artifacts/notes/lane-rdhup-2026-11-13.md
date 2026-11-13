# lane/rdhup 2026-11-13 - FIXED
TCP poll in servers/net now reports POLLRDHUP on peer FIN and POLLHUP when both directions are done (tcp_peer_fin, state-based: smoltcp may_recv() stays true in CLOSE-WAIT while unread data remains, so tcp_rx_eof hid the FIN).
Guard: ip6test ns_* cases (peer shutdown(SHUT_WR) -> POLLIN|POLLRDHUP, no POLLHUP; both shut -> POLLHUP). Red before fix (errno 16, 5 cases), green after; ping6_ll_router fails in both (pre-existing).
Box regress aarch64 + x86_64 PASS. epoll path passes poll bits through unchanged (kernel masks state & requested).
