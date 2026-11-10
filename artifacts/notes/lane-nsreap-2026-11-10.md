# lane/nsreap 2026-11-10 - FIXED

- Root cause: not TCP. `nlsock::NL` (netlink sockets) holds a counted `NsRef` on its netns and was only pruned
  of dead descriptions inside `nlsock::create` (the next new netlink socket). A netns whose process opened a
  netlink socket (every namespace that configured eth0 before listening) stayed pinned after the process exited,
  so `GONE` never fired and the veth ends stayed. UDP/ICMP/no-socket namespaces in veth_nsgone never opened one.
  A never-closed listener alone does not pin it.
- Fix: `nlsock::prune()` (drops entries whose ofd is not live, references dropped outside the NL lock), called at
  the start of `vif::daemon_pass` (daemon tick and each rtnl request).
- Tests (brtest): veth_pair reap assertion restored; veth_tcp_reap; veth_listen_reap.
  Fail-first on unfixed main: 30 passed / 3 failed (those three); fixed: 33/0.
- Side finding (not changed): an accepted TCP socket's SockEntry gets `ns: NsRef::INIT` (netns 0), servers/net/src/lib.rs accept path.
