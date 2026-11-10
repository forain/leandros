# lane/acceptns 2026-11-10 - FIXED

Accepted inet TCP SockEntry was created with `ns: NsRef::INIT` (servers/net/src/lib.rs); now clones the listener's NsRef (Linux sk_clone).
Audited other creators: socket() and socketpair use the caller netns, unix accept already used the listener's netns, dup clones the entry. No other site affected.
Guard: brtest accept_ns (accept in a child netns, pass the fd to the host via SCM_RIGHTS, child exits; the netns/veth must survive while the fd is open, data still flows, veth reaped after close).
Fail-first on main: 2 FAILs (netns pinned..., still carries data). With fix: brtest clean; full aarch64 box regress 81 suites 0 bad.
