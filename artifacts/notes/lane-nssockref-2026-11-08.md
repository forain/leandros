# lane/nssockref 2026-11-08 (ffsbreview1 finding 3)

Problem: servers/net `SockEntry.netns` was a bare `u16` NsId with no reference. After every task of a
netns exited the id was freed and could be reused by a new namespace of any kind (shared id table),
which then saw the old abstract-unix listeners (and port bindings) keyed by that id.

Fix: `SockEntry.ns: sched::ns::NsRef` (accessor `netns()` returns the id). `SockEntry` and `XferFd`
lost `Copy` (now `Clone`); every duplicate site clones (a clone takes a ref), every free site is an
assignment of `SockEntry::empty()` / Vec drop, so `Drop` releases the ref automatically. Refs are taken at
socket(), socketpair(), accept (from the listener), fork copy, dup, SCM_RIGHTS export/import
(in-flight `XferFd::Sock` holds its own ref), pidfd_getfd. No ns.rs change.

Tests (userland/nstest): `sock_ns_reuse` (child netns binds abstract name, passes fd to parent over
SCM_RIGHTS, exits; 80 new net/user namespaces must get ECONNREFUSED; original listener still accepts
(EAGAIN) and has its abstract name) and `sock_ns_leak` (200 short-lived netns x ~11 sockets, ns table of
256 must not exhaust).
Before the fix (main servers/net): `FAIL sock_ns_reuse` - "stale abstract name seen by new ns #0".
After: nstest 54 passed, 0 failed.
