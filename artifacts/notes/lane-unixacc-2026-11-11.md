# lane unixacc 2026-11-11 — REFUTED (both TODO items already fixed; stale)
- Closed-before-accept: servers/net/src/lib.rs keeps the embryo on the listener backlog via `pending_sid`/`unix_backlog_head`
  (accept returns it, data then EOF). Fixed in ccf90b47; guarded by udptest `unix_accept_closed` (netabi.c) and chroottest.
- Pool: `UNIX_CONNS` is a heap Vec grown on demand to MAX_CONNS=8192 (lane pools, 2026-10-19), guarded by procmax `unixconns` (1200 socketpairs).
- Evidence: box aarch64 regress of /bin/procmax /bin/udptest on d831fc3f: 2 suites, 0 bad, PASS.
- No code change; TODO.md entries struck. Remaining limit: system-wide 8192 cap (not per-netns) — acceptable.
