# Networking

[`servers/net`](../../servers/net) provides two largely separate socket families behind one fd range: **AF_INET** — TCP, UDP and ICMP over virtio-net or a real Realtek NIC through [smoltcp](https://github.com/smoltcp-rs/smoltcp), with a DHCPv4 client and a separate loopback interface — and **AF_UNIX**, implemented in full because every Wayland and D-Bus connection in the system rides on it. Both are driven by Linux syscalls with Linux semantics; the reference for every behaviour below is what Linux's own `net/` code does.

## Overview

| Piece | Summary |
|---|---|
| Socket tables | one `ProcSockTable` per process, heap-allocated on first use and reused; its slots are a growable vector. Slot `i` is handle `SOCK_FD_BASE + i` (`0x0100_0000 + i`), which a process reaches only through a `SockAlias` entry of its fd table — every socket is an ordinary descriptor at the lowest free number |
| AF_INET | two smoltcp `Interface`s: the NIC (`NET_STACK`) and loopback 127.0.0.0/8 (`LO_STACK`) |
| DHCP | DHCPv4 on the NIC; the lease writes `/etc/resolv.conf` (`# from DHCP`) |
| AF_UNIX | connection pool of 512 (`UNIX_CONNS`), bound-address pool of 512 (`BOUND_PATHS`), growable per-direction rings 4 KiB → 208 KiB |
| Pathname sockets | `S_IFSOCK` nodes on tmpfs only ([vfs.md](vfs.md#tmpfs)) |
| `/proc/net/*` | Linux-format `route`, `dev`, `tcp`, `udp`, `unix`, plus header-only `tcp6`, `udp6`, `raw`, `raw6` ([`procnet.rs`](../../servers/net/src/procnet.rs)) |
| Daemon | `net_daemon` sleeps until smoltcp's next timer (microsecond stack clock), runs at 1 kHz while frames flow and 100 Hz when idle, and is woken by `netdev::kick()` on sends, window reopening and the r8169 RX interrupt |
| NIC | `eth0` is virtio-net if present, else the Realtek r8169 ([pc.md](../hardware/pc.md#ethernet-realtek-r8169)); `/proc/net/nic` reports the driver state; DHCP restarts on link up |
| TCP buffers | 2 MiB receive window (window scaling), 256 KiB send buffer (smaller past 48 sockets, fallible allocation); UDP 512-slot receive buffer |

Host↔VM networking works in both directions on every accelerator; `ping` is in the userland, and `nettest` is part of the `regress` suite.

## TCP/IP

### How it works

- **Two interfaces, two socket sets.** Loopback is deliberately a second smoltcp `Interface` with its own `SocketSet`, not a second address on the NIC. Two interfaces sharing one set would let whichever polls first claim a socket's pending segment, so a loopback SYN could be emitted onto the wire and never delivered. Separate sets make the choice explicitly, once, at `bind`/`connect` time. The loopback stack is unconditional — it exists even with no NIC.
- **Binding.** Port 0 is assigned at `bind` (not at `listen`), which is what makes `bind("127.0.0.1:0")` — every mio/tokio `TcpListener::bind` — work. A wildcard listener listens on both stacks. Ports below 1024 require root (`EACCES`).
- **TIME_WAIT.** A closed connection's port is reserved for 60 s (Linux's fixed `TCP_TIMEWAIT_LEN`): an explicit `bind` to it is `EADDRINUSE` unless `SO_REUSEADDR` is set, and automatic ports skip it. Only the *port reservation* is modelled — the smoltcp socket is still torn down at close, so nothing lingers to re-ACK a retransmitted FIN. Under pressure (64 reservation slots) the newest reservation is simply not recorded, so the failure mode is the old instant reuse, never an unsatisfiable `bind`.
- **Options.** `TCP_NODELAY`, `SO_KEEPALIVE`, `TCP_KEEPIDLE`/`KEEPINTVL`/`KEEPCNT` take effect (defaults 0/7200/75/9, Linux's ranges and `EINVAL`s). They live in a side map keyed by **open file description**, applied at `connect`, at `accept` (an accepted socket inherits them from its listener, as Linux's `sk_clone_lock` does), on `setsockopt` of a connected socket, and when the handshake completes. The TCP level on a non-TCP socket answers like Linux: `EOPNOTSUPP` for AF_UNIX, `ENOPROTOOPT` for UDP/ICMP/raw.
- **Peeking.** `MSG_PEEK` leaves data queued on TCP (`peek_slice`) and UDP (`peek`), for `recv`, `recvfrom` and `recvmsg`.
- **Half-close.** `shutdown(SHUT_WR)` delivers EOF to the peer while the reply still arrives; `send` after it is `EPIPE`; `shutdown` of a listener or an unconnected socket is `ENOTCONN`.

### Shared sockets

An inet (or ICMP) socket's object is a set of smoltcp handles named by value from its `SockState`; unlike an AF_UNIX end it has no refcount of its own. Every holder — the creator, a `dup`, a fork child, an `SCM_RIGHTS` receiver, a `pidfd_getfd` caller — keeps its own copy of the entry, and what ties the copies together is the **open file description** they share (`SockEntry::ofd`):

- **The description's refcount is the socket's.** The holder whose put drops it to zero tears the handles down (`inet_put`, counted under `SOCK_TABLES` so that last holder sees what the others published). A socket that got no description (the table was full) is never shared: fork, `dup` and `SCM_RIGHTS` refuse it.
- **State changes reach every copy.** `bind`, `listen`, `connect` and its outcome, `accept` re-arming the listener, `SO_REUSEADDR` and an ICMP socket's first send are copied into every table entry of the description and into a canonical row (`share_state`); a copy queued by `SCM_RIGHTS` is refreshed from that row when it is received or dropped.
- **`accept` is race-safe.** smoltcp has no accept queue: the listening socket *becomes* the connection and a replacement is armed. `accept` checks, under `SOCK_TABLES`, that the listener still names the connection it took; a second holder (or thread) that took the same one drops the listener it armed and sees `EAGAIN`.

Until lane fdgaps (2026-10-13) a second copy meant a double free of the handles, so fork skipped inet and listening sockets (the child had no descriptor for them), `dup` answered `EINVAL` and `pidfd_getfd` / `SCM_RIGHTS` `EBADF`. Fork also inherits AF_UNIX listeners now (one more reference on the bound address). `syscalltest`'s `inet_fork_shared` and `pidfd_getfd_inet` cover it.

### ICMP and ping

Unprivileged `socket(AF_INET, SOCK_RAW)` is `EPERM`, as on Linux; `ping` ([`userland/ping`](../../userland/ping)) falls back to an `IPPROTO_ICMP` datagram socket. smoltcp's ICMP socket has no peek, so the server keeps **one stashed datagram per socket** (`icmp_peeked`, under the stack's own lock — no new lock, no lock-order edge): a peek stashes what it dequeues, a real read takes it, `FIONREAD` reports its length, readiness counts it, and closing the socket drops it (smoltcp reuses handles, so a stale stash would leak into the next socket). ICMP shares the UDP datagram path, so `recvmsg`/`sendmsg` move one datagram over all iovecs and fill `msg_name`; `SO_PROTOCOL` reports `IPPROTO_ICMP`.

`ping` waits with `poll()` bounded by the per-packet timeout and then reads with `MSG_DONTWAIT`. It used to assume `recvfrom` was non-blocking; the kernel parks a blocking `recvfrom` until data arrives, so with no reply `ping` slept forever and never printed "Request timeout". It supports `-c`, `-W`, `-i` and prints packet loss. Signals do end a blocked `recvfrom` (`EINTR`).

### Design decisions and refuted premises

- **"aarch64 ping hangs" was the host, not the architecture.** On a Mac using `socket_vmnet`, the guest's gateway is the vmnet gateway, not QEMU's slirp address `10.0.2.2`; a ping to `10.0.2.2` leaves through the host's default route and is never answered — on x86_64 too. Forcing slirp (`LEANDROS_NET=user`) made the same image answer 4/4. The genuine bug found on the way was `ping`'s missing timeout.
- **"aarch64 on slirp never gets a DHCP lease" was the harness.** QEMU's socket chardev discards output while no client is connected; the harness disconnected at `login: `, which on a fast accelerator is exactly when the lease line printed. `/etc/resolv.conf` proved the lease every time. The harness now always keeps a full serial log from QEMU's own chardev logfile.
- **"The PC's 2 Mbit/s download" was a 64 KiB window and a polled daemon** (lane netperf). Measured on the real PC: 4464 frames in 24.7 s, nothing dropped, over a ~270 ms round trip, which is exactly one 64 KiB window per RTT; and the 100 Hz daemon added 10-20 ms to every round trip, capping even a host on the same machine at ~21 Mbit/s. The fixes are the 2 MiB window and send buffer, the interrupt/kick-driven daemon (it sleeps until smoltcp's next timer), per-socket readiness (inet sockets report a poll tag and the daemon wakes only the sockets whose readiness moved in a pass, with a broadcast at most every 100 ms as a safety net), and 256 virtio-net RX buffers (was 16) with a batched RX doorbell. QEMU aarch64/HVF host HTTP went 21.6 to 2288 Mbit/s, CDN 8.0 to 31.3; x86_64/TCG 19.5 to 514; gateway ping 19.5 to 3.0 ms; on the PC, 77 and 166 Mbit/s in the first two measurements. `netcheck` measures it (below).
- **`TCP_NODELAY` shows no measurable effect on loopback, and that is correct** (a sub-10 ms RTT: the ACK arrives before Nagle's hold matters). It matters against internet peers. The test that proves the options reach the wire counts NIC packets during idle with keepalive on (≥3 more than without).
- **Keepalive is an approximation.** smoltcp has one keepalive interval and one timeout. The mapping is interval = `keepidle` and timeout = `keepidle + keepintvl × keepcnt`, so a silent peer is dropped at exactly Linux's deadline — but probes repeat every `keepidle` rather than every `keepintvl` (with defaults, one probe before the deadline instead of nine), the timeout also covers in-flight data, and it is not armed during the handshake. Enabling keepalive on an idle socket sends one probe immediately.

## AF_UNIX

### What is implemented

- `SOCK_STREAM`, `SOCK_DGRAM` and `SOCK_SEQPACKET`; pathname sockets bound to real VFS nodes (tmpfs only) and Linux abstract-namespace addresses; `socketpair`.
- **SCM_RIGHTS** fd passing (up to `SCM_MAX_FD` = 253 per message) and cmsg flags (`MSG_CTRUNC`, `MSG_CMSG_CLOEXEC`). VFS descriptors and lifted socket entries travel through one `XferFd` transfer type, so a socket fd — including a *listening* one — can be passed. A sender that would push a connection's in-flight fd count past 1024 gets `ETOOMANYREFS`.
- **`SO_PEERCRED`** with each end's credentials captured at connect time (D-Bus `EXTERNAL` auth and init's power-off authorisation depend on it).
- `listen()` under Linux's own gates, in Linux's order: a type that cannot accept is `EOPNOTSUPP` (checked before the address, so even a bound `SOCK_DGRAM` socket gets 95), an unbound or connected socket is `EINVAL`.
- `accept4()` honouring `SOCK_NONBLOCK`/`SOCK_CLOEXEC`, well-formed peer sockaddrs from `accept()`, `F_SETFD`/`F_GETFD` on socket fds, `FIONBIO`, `FIONREAD`/`TIOCOUTQ`.
- **`connect` checks permission**: search on every directory of the path and write on the socket inode, as on Linux (root bypasses).
- Writes on a connected-but-not-yet-accepted stream are buffered; pending connections survive `fork` and are refcounted on close; connections stay alive until the last fd reference closes.
- A full send ring returns `EAGAIN`, never `0`.
- **`shutdown(2)` is a per-direction half-close**, not a teardown: `SHUT_RD` makes later receives report EOF and the peer's sends raise `EPIPE`; `SHUT_WR` the reverse; neither invalidates the fd.
- `dup` of a listening socket works: bound addresses are refcounted, so two fds share one listening queue and the address survives until the last closes.
- `read()`/`recv()` on a stream with queued fds stops at the fd batch's boundary and closes the fds when the call has no control buffer (`recvmsg` sets `MSG_CTRUNC`); `MSG_PEEK` copies bytes without consuming them and, with a control buffer, installs new references to the batch's fds while leaving it queued.
- **Garbage collection of in-flight cycles** (`unix_gc`, Linux's `net/unix/garbage.c`). An end whose only references are its own queued in-flight copies is a candidate; everything reachable from live ends' receive queues is marked; unreached candidates' queues are purged, which frees the connections through the normal close path. Listeners are graph nodes too, with their embryonic connections as edges, so a listener passed into its own backlog is collected and its address released. Without it, a socket sent over itself leaked a connection slot forever (600 rounds exceeded the 512-slot pool).

### Design decisions, and how the bugs presented

The AF_UNIX work is the clearest example of the project's "match Linux exactly" rule paying for itself, because the symptoms appeared many layers above the faults.

- **`0` instead of `EAGAIN` desynchronised Wayland.** A stream `send` on a full 4 KiB ring returned `0`, which a client cannot distinguish from a zero-length write. libwayland busy-looped in `wl_connection_flush` with its tail unadvanced, and under cosmic-panel's heavy multi-fd traffic the stream was corrupted — surfacing as an inexplicable `Unknown id 636` protocol error in the compositor. It is very likely also the original "slow vs. stuck" ghost of an early desktop milestone.
- **`shutdown` ignored `how`.** It emptied the fd-table slot, which made `shutdown(fd, SHUT_WR)` *strictly more destructive than `close(fd)`*. tokio issues `SHUT_WR` from `OwnedWriteHalf::drop`; cosmic-session's environment handshake depended on it, and for weeks the stall was recorded as a "tokio integration residual" and worked around with a 5 s timeout patch to cosmic-session. Fixing the kernel let the patch be deleted and the vendored source restored to upstream; the component cascade now starts 2.186 s after `Starting cosmic-session` instead of 5.022 s.
- **A failed `dup` killed the panel.** `wp_security_context_v1.create_listener`, which cosmic-panel calls for any applet with `X-HostWaylandDisplay=true`, marshals a *listening* socket, and libwayland dups every fd it marshals. `dup` of a listener was `EINVAL`, and a marshalling failure poisons libwayland's *whole* connection, so the error propagated out of `event_loop.dispatch()` and cosmic-panel exited 1. The bar vanished entirely — which reads as "the applets did not render" and is nothing of the kind. The same path also needed SCM_RIGHTS to carry socket fds at all, and pending connections to survive the `fork` that `launch-pad` performs at the moment an applet's privileged socket is pending.
- **The ring grows instead of staying at 4 KiB.** A fixed 4 KiB ring filled in about 100 ms under cosmic-workspaces' ~40 KB/s capture loop whenever cosmic-comp was briefly busy, and the client's `flush().unwrap()` panicked on `EAGAIN`. Rings now grow on demand (linearised, doubling) up to Linux's default `net.core.wmem_default` of 212 992 bytes and drop back to nothing once drained.

### Limits

The connection pool (512) and bound-address pool (512) are global; a COSMIC session holds about 110 connections and each terminal adds about 5. When the pool is empty the server prints `[NET] unix connection pool FULL` once. Socket tables per process grow with use (they were 455 entries in one 32 KiB order-3 block until lane fdmax); per-socket state that only some sockets need still goes in side maps rather than `SockEntry`.

## /proc/net

[`procnet.rs`](../../servers/net/src/procnet.rs) generates the Linux formats so stock tools work: `route` (default-gateway discovery the way `route -n`/`netstat -r` do it), `dev` (per-interface counters), `tcp`, `udp`, `unix`. IPv4 addresses are `%08X` of the network-order word; the `inode` column is the socket's open-file-description id (there is no sockfs); `uid` is the effective uid of the owning process. The VFS calls the generator on every open and parks the bytes in an ephemeral snapshot.

## Limitations and open work

- **No IPv6, netlink or packet sockets** (`EAFNOSUPPORT`). Firefox and musl fall back cleanly.
- **virtio-net RX has no interrupt** (the daemon is woken by sends and polls at 100 Hz idle, 1 kHz while frames flow); r8169 has a real RX interrupt. **r8169 RX stalls (fix needs PC confirmation):** on the PC (boot 16) the receive path stalled and the watchdog reset the chip at about 235 s and 245 s of uptime, both times with RxDescUnavail and RxFIFOOver latched in `IntrStatus` (`00d4`) and enabled in `IntrMask` (`807b`) but no overflow interrupt counted. That is a dead MSI: the ISR masked only RxOK and acked only the bits it had read, so an enabled event latched in between (FIFO overflow right after descriptor-unavailable) kept `IntrStatus & IntrMask` non-zero and no later event made a new MSI edge; nothing else acked those bits. Lane r8169irq made the protocol Linux's (ISR: mask all, ack exactly what was read, kick; `rx_rearm`: unmask, re-read status, poll again if enabled work is pending, ack latched bits outside the mask; RxDescUnavail/RxFIFOOver no longer enabled on 8168 parts and never a reason to reset), modelled in `drivers/r8169-tests`. The watchdog stays as the last resort and now tells a starved daemon (frames waiting: no reset) from a stopped chip (soft recovery, then reset). Grep the PC log for `[r8169] stats` (`irq recheck rescued`) and `RX stall` / `RX backlog` ([pc.md](../hardware/pc.md#ethernet-realtek-r8169), [TODO.md](../../TODO.md)).
- An **AF_UNIX** socket shared by fork or `dup` before it is bound or connected does not share what happens next: the holder that binds or connects changes only its own copy (its bound address and connection end are refcounted per holder, so `share_state` copies inet states only).
- `O_NONBLOCK` is kept per entry, not per open file description.
- `SO_ERROR` reports only a refused connect, not reset, timeout or unreachable.
- No conflict check against *live* bound ports (only TIME_WAIT reservations); no `SO_REUSEPORT` or `SO_LINGER`.
- `getsockopt` answers `ENOPROTOOPT` for `SO_ACCEPTCONN`, `SO_LINGER`, `SO_RCVTIMEO`, `TCP_INFO` and the `IP_*` options; add each when a real caller needs it. `TCP_USER_TIMEOUT` is not implemented.
- Clearing `TCP_NODELAY` on a connected socket does not flush (moot: the next tick sends anyway).
- A raw ICMP receive returns the ICMP message without Linux's 20-byte IPv4 header; `/proc/net/raw` has no ICMP rows.
- `mknod(S_IFSOCK)` is `EPERM` for everyone (Linux allows it unprivileged).
- An nftables-style rule engine ([`nftables.rs`](../../servers/net/src/nftables.rs)) is evaluated on the prerouting and output hooks, but nothing can load rules into it, so it always accepts. There is no `/dev/net/tun`.

## Code and tests

- [`servers/net/src/lib.rs`](../../servers/net/src/lib.rs) — sockets, both stacks, AF_UNIX, GC, options.
- [`servers/net/src/procnet.rs`](../../servers/net/src/procnet.rs) — `/proc/net/*`.
- [`drivers/src/virtio_net.rs`](../../drivers/src/virtio_net.rs), [`r8169.rs`](../../drivers/src/r8169.rs), [`netdev.rs`](../../drivers/src/netdev.rs) — the NICs and the backend dispatch.
- `userland/netcheck` (run by `/bin/net-autotest` when init sees `net.test=1`) — NIC chip, XID and MAC, link speed, the DHCP lease, DNS, a gateway ping, a TCP connect, HTTP GETs, and download throughput: a single stream from a CDN with per-second rates, a far server, and four parallel streams; `netcheck --get URL [--par N] [--secs S] [--mb M]` for manual runs. Prints `NETCHECK <step>: PASS|FAIL` lines and a summary.
- [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs) — socket syscall dispatch (`not_a_socket`: a socket call on a VFS-range fd or on an epoll fd held by the caller is `ENOTSOCK`, as on Linux; anything else `EBADF`).
- `userland/nettest` — ICMP raw/dgram, non-blocking, poll timeout, `EINTR`, peek/recvmsg/sockopt, UDP and TCP DNS, `getaddrinfo`, TCP half-close, NODELAY/keepalive and keepalive probes on the wire; it discovers the gateway from `/proc/net/route` and the resolver from `/etc/resolv.conf`, so the `regress` suite needs host internet access. As uid 1000, `icmp_raw` is skipped (`EPERM`, as on Linux).
- `userland/scmtest` — SCM_RIGHTS, `SO_PEERCRED`, full-ring `EAGAIN`, listen gates, TIME_WAIT, the GC cycles, `read_discards_fds`, `inet_msg_peek`, `send_on_epoll_enotsock`. Capture its output with `scripts/scmrun.py`: a harness heuristic once mistook a `->` in its diagnostic output for a prompt and reported a hang that did not exist.
- `userland/wakepolltest`, `userland/polltest` — readiness of sockets in poll/epoll.

## See also

- [vfs.md](vfs.md) — socket nodes on tmpfs, open file descriptions, `SockAlias`.
- [security.md](security.md) — `connect` permission, privileged ports, raw sockets, init's `SO_PEERCRED` authorisation.
- [../kernel/event-loop.md](../kernel/event-loop.md) — `NET_POLL` readiness and edges.
- [../desktop/session.md](../desktop/session.md) — the Wayland and D-Bus traffic that drove most of this.
- [../getting-started/running.md](../getting-started/running.md) — host network backends (slirp, vmnet).
