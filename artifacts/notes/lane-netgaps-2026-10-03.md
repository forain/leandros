# lane/netgaps — 2026-10-03

Branch `lane/netgaps`, worktree `.claude/worktrees/netgaps`, base `f994928` (main). Not merged, not pushed to main.

All testing was done on the Mac: aarch64 on HVF, x86_64 on TCG, each under socket_vmnet (the default) and slirp (`LEANDROS_NET=user`). Neither Linux machine was reachable (desktop 172.16.158.150 and its Tailscale address, and the laptop 172.16.149.179, all timed out), so nothing ran under KVM.

## Commits
- `b4f694c` nettest: ICMP peek/recvmsg/sockopt and TCP half-close cases
- `d74cf10` net: ICMP sockets keep a peeked datagram; recvmsg/sendmsg move one datagram
- `98f2a37` runtests: nettest in the regress suite, and a net suite
- `791ea4b` run-leandros: keep a complete serial log from QEMU's chardev logfile

## Gap 1: raw ICMP MSG_PEEK consumed data (FIXED)
**Root cause.** smoltcp 0.11's `icmp::Socket` has `recv()` and no peek, and `handle_recv` ignored `MSG_PEEK` for ICMP. Two more defects sat on the same path:
- `recvmsg` sent ICMP down the stream fallback. That path reads each iovec as a *separate datagram* and never writes `msg_name`, because `inet_dgram()` excluded ICMP.
- FIONREAD answered 0.

Smaller defects fixed in the same commit:
- `SO_PROTOCOL` on a ping socket said 17 (UDP).
- `recv` on a ping socket that had not sent anything returned EBADF.
- `sendmsg` split a datagram per iovec.

**Fix** (`servers/net/src/lib.rs`):
- `NetStack.icmp_peeked` is a `BTreeMap<SocketHandle, (Vec<u8>, IpAddress)>`. It holds one datagram per socket. It lives under the stack's own lock, so it adds no new lock and no lock-order edge.
- `icmp_recv_locked(s, h, cap, peek)` is the single read primitive:
  - It serves the stashed datagram first.
  - A peek stashes the datagram it dequeues.
  - A real read takes the datagram and clears the stash.
- `icmp_readable()` treats a stashed datagram as POLLIN.
- FIONREAD returns the next datagram's length. It calls `icmp_recv_locked(.., peek=true)`, which also stashes the datagram.
- `release_inet_socket` drops the stash. smoltcp reuses socket handles, so a stale stash would otherwise leak into a new socket. The per-fd ICMP close now goes through `release_inet_socket`.
- `inet_dgram()` now includes ICMP, so `recvmsg` and `sendmsg` take the UDP datagram path: one datagram over all the iovecs, the source in `msg_name`, and MSG_TRUNC. `udp_recv_k` gained an `IcmpBound` arm.
- The ICMP send arms accept the gathered `kdata`.
- `SO_PROTOCOL` returns `IPPROTO_ICMP`, and `recv` on `IcmpUnbound` returns EAGAIN.

**Tests** (nettest): `icmp_peek`, `icmp_recvmsg`, `icmp_sockopt`.
- **Before:** on both arches, under both vmnet and slirp (`tests-before{,2}-a64-*`, `tests-before-x86-*`), all three cases fail:
  - `icmp_peek: FAIL second peek returned -1 bytes (first: 40)`
  - `icmp_recvmsg: FAIL recvmsg returned 8 bytes over [8, 248] iovecs (want one whole 40-byte reply, seq 101)`
  - `icmp_sockopt: FAIL ... SO_PROTOCOL 17`
- **After:** the same three runs are 0 failures.
- The new cases were also checked against the macOS host kernel (`cc nettest.c`). There, BSD's FIONREAD counts address records, which the test allows under `__APPLE__`. Linux's FIONREAD on a ping socket is ENOTTY (ping_prot has no ioctl), and the test accepts an unsupported FIONREAD. Ours returns the datagram length, the way UDP and raw sockets report it.

**Note for lane/ffaudio:** this lane touched no AF_UNIX code. `inet_dgram()` only matters for sockets that are not unix stream sockets. The edits are in the ICMP arms, `udp_recv_k`, `handle_queue_len`, the poll arm, `release_inet_socket` and the `SO_PROTOCOL` row of getsockopt. If ffaudio also edits `handle_getsockopt`, expect a textual merge conflict near the `SO_PROTOCOL` line.

## Gap 2: nettest needed `-g` and was not in any suite (FIXED; mostly already done)
`/proc/net/{route,dev,tcp,udp,unix}` (plus header-only tcp6/udp6/raw/raw6) had already landed in `24bf6e8`. nettest already read the gateway from `/proc/net/route` and the DNS server from `/etc/resolv.conf`, so the a64ping note was stale. What was left was wiring the test into the suite runner:
- `runtests.py` `regress` now ends with `/bin/nettest`.
- A new `net` suite runs nettest alone.

Results, with no flags:
- aarch64 vmnet: gateway 192.168.105.1, 17 pass, 1 skip (`tcp_http` needs `-t`).
- aarch64 slirp: gateway 10.0.2.2, DNS 10.0.2.3, same result.
- x86_64 vmnet and x86_64 slirp: same result.
- As `leandro` (uid 1000) on aarch64: 16 pass, 2 skip (`icmp_raw` gets EPERM, as on Linux).

The DNS cases resolve example.com, so the regress suite now needs the host to have internet access.

## Gap 3: "aarch64 on slirp never prints [NET] DHCP configured" (STALE; harness artifact, harness fixed)
**Evidence:**
- On the base kernel with aarch64 and slirp, under both HVF and TCG, `/etc/resolv.conf` reads `# from DHCP / nameserver 10.0.2.3`, and the line is in the serial log. `vfs::set_dhcp_dns` and the print sit in the same `Configured` branch.
- Under vmnet, aarch64 serial logs still lacked the line, even though resolv.conf said `# from DHCP / nameserver 192.168.105.1`.
- I added `logfile=` to the serial chardev. QEMU's own log then shows `login: [NET] DHCP configured, address: 192.168.105.75`, on the same line as the prompt.

**Cause.** QEMU's socket chardev discards output while no client is connected. `driver.py start` disconnects when it sees `login: `, and on HVF the vmnet lease lands exactly then. x86_64 on TCG gets its lease before the prompt, so its line survives.

**Fix.** The chardev now always logs to `/tmp/leandros[-RUN_ID]-serial-full.log`, and runtests copies it as `serial-full.log`. TODO.md's "vmnet gotcha" paragraph is marked CLOSED with this evidence. `tests-final-a64-slirp/serial-full.log` has the line.

## Gap 4: sweep of other networking gaps
**Fixed or covered here:**
- **TCP `shutdown()` / `tcp::Socket::close()` path untested, ENOTCONN untested** (TODO.md ~L2268). New nettest case `tcp_shutdown` over 127.0.0.1 checks:
  - SHUT_WR delivers "ping" then EOF to the peer.
  - The peer's reply still arrives.
  - `send` after SHUT_WR returns EPIPE.
  - `shutdown` of a listener and of an unconnected socket returns ENOTCONN.

  It passes on the base kernel too. This was a coverage gap, not a bug.
- **ICMP `SO_PROTOCOL` reported UDP; ping-socket recv returned EBADF before any send.** Fixed with gap 1.

**Deferred, with recommendations:**
| Gap | Source | Recommendation |
|---|---|---|
| IPv6 (`AF_INET6`), `AF_NETLINK`, `AF_PACKET` all return EAFNOSUPPORT | lane-firefox-2026-09-27.md:134 | Large. Firefox and musl fall back cleanly. Do this only when something needs it. |
| virtio-net RX has no IRQ; the net daemon polls at 100 Hz | lane-greeterdisp-2026-09-24.md:75 | Make the poll adaptive with smoltcp `poll_delay()`, and skip polling when there are no inet sockets. Medium-small, and it cuts about 100 idle wakeups/s. A real IRQ path is a separate, larger lane. |
| `TCP_NODELAY` / `SO_KEEPALIVE` are accepted but do nothing (they read back 0) | lib.rs getsockopt comment | Apply them to the smoltcp socket on a connected socket (`set_nagle_enabled`, `set_keep_alive`). Options set before connect need a per-entry flag, and `SockEntry` is at its 32 KiB table budget. That is why this was not done here. Worth doing for Firefox latency (NSPR sets TCP_NODELAY). |
| `dup()` of a connected or listening inet socket returns EINVAL | lib.rs `handle_dup` | Needs a refcount on inet entries, like unix `refs_a`/`refs_b`. Medium. |
| `SO_ERROR` reports only a refused connect (not reset, timeout or unreachable) | lib.rs getsockopt | Medium. Record a per-socket last error when smoltcp goes to Closed. |
| getsockopt ENOPROTOOPT for `SO_ACCEPTCONN`, `SO_LINGER`, `SO_RCVTIMEO`, `TCP_INFO`, `IP_*` | lib.rs | Cheap per option. Add each one when a real caller hits it; none is known to today. |
| No EADDRINUSE check against *live* bound ports; no `SO_REUSEPORT`/`SO_LINGER` | m9-todo-reconciled.md:753 | The live-port check is cheap but changes bind semantics for every server. Do it with a test in its own lane. |
| `/proc/net/raw` (and `/proc/net/icmp`) have no rows for ICMP sockets | lane-polish1002 | Cheap. Low value until something reads them (`netstat -w`, `ss -w`). |
| Linux `SOCK_RAW` ICMP recv includes the IPv4 header; ours returns ICMP only | found here | Cheap-medium (synthesize a 20-byte header). busybox and iputils ping and nettest handle both forms. Do it if a raw-socket tool misparses. |
| socket syscalls on an epoll-range fd return EBADF, not ENOTSOCK | coordinator, after ffaudio `94a2994` | Cheap, but it belongs in `kernel/src/syscall.rs` `dispatch_inner`, which lane/ffaudio just changed for ENOTSOCK. Do it on top of that commit once it merges, to avoid a conflict. |
| TCP 64 KiB window never measured | lane-firefox-2026-09-27.md:143 | Measurement only: an iperf-style loopback and NIC throughput test. |
| Occasional first DNS query lost (both arches booted at once) | lane-firefox-2026-09-27.md:80 | Likely host vmnet/NAT. Investigate only if it reproduces with one guest. |

## Verification
- Full `runtests` regress, 15 commands including nettest, with the fixed kernel:
  - aarch64 vmnet: PASS (`/tmp/netgaps/tests-regress-a64`)
  - x86_64 vmnet: PASS (`tests-regress-x86`)
  - aarch64 slirp: PASS (`tests-final-a64-slirp`)
  - x86_64 slirp: PASS (`tests-final-x86-slirp`)
- Firefox https smoke test (ffsession `--url https://example.com/`): "Example Domain" rendered over https on aarch64 (`/tmp/netgaps/run-ff-a64/ff-1.png`) and on x86_64 (`/tmp/netgaps/run-ff-x86/ff-3.png`).
- Mistake made here: early runs used run IDs `netgaps-a` and `netgaps-x` with the default VNC port. That broke a boot in the ffaudio lane. For parallel lanes, always set `LEANDROS_RUN_ID` **and** a unique `LEANDROS_VNC_PORT`. This lane's port is 5963.
