# lane/tcpopts: 2026-10-03

Branch `lane/tcpopts`, worktree `.claude/worktrees/tcpopts`, base `0a52161` (origin/main). It is pushed to origin. It is not merged.

All runs were on the Mac:
- aarch64 on HVF.
- x86_64 on TCG.
- Both under socket_vmnet, with `LEANDROS_RUN_ID=tcpopts LEANDROS_VNC_PORT=5966`.

The desktop (172.16.158.150) timed out on ssh, so nothing ran under KVM.

## Commits
- `0bf6cb5` net: TCP_NODELAY and the keepalive options take effect
- `cb81061` nettest: tcp_nodelay, tcp_keepalive and tcp_keepalive_probes cases
- this note

## Problem
`handle_setsockopt` returned `ok_reply()` for TCP_NODELAY and SO_KEEPALIVE and dropped them. getsockopt hard-coded 0 for both. Nothing reached smoltcp.

## Design: a side map keyed by open file description
**Size numbers.**
- `SockEntry` is 72 bytes:
  - `SockState`: 56 bytes. Its largest variant is `InetListening`: two `Option<SocketHandle>` plus an `IpEndpoint`.
  - Eight 1-byte fields, `bound_port` (u16) and `ofd` (u32).
  - 2 bytes of padding.
- `ProcSockTable` holds 455 entries, which is 32 760 bytes, plus `pid` and the flag. That is exactly the 32 KiB order-3 budget, asserted at compile time.
- The options need 7 bytes: two flags, keepidle u16, keepintvl u16, keepcnt u8.

**Options considered.**
| Option | Verdict |
|---|---|
| Fit the options into the entry's 2 padding bytes | Not enough room. Only the two flags would fit, and idle/intvl/cnt would still need storage somewhere else. |
| Pack the six `bool`s into a bitfield to free 5 bytes | It would fit, but it touches every `in_use`/`cloexec`/... site in a 5 000-line file that other lanes edit. |
| Apply the options lazily when the smoltcp socket is created | This still needs storage from `socket()` until `connect()`/`accept()`, so it is not a storage design by itself. It is used here as the *apply* strategy. |
| **Side map `SOCK_OPTS: Mutex<BTreeMap<u32 ofd, SockOpts>>`** | **Chosen.** |

**Why the side map.**
- **Zero bytes added to `SockEntry`.** `MAX_SOCKS` stays at 455.
- **It is the correct key.** On Linux the options belong to the socket, and dup, fork and SCM_RIGHTS copies share the open file description. Keying by `ofd` gives that sharing for free. A per-entry field would diverge after fork.
- **Only sockets with non-default options have a row.** Setting all options back to default removes the row.
- **Cleanup without new hooks.**
  - Inserting a new key first runs `retain(|k| vfs::ofd::live(k))`. That bounds the map by the live sockets that changed an option.
  - ofd ids carry a 16-bit generation, so a dead row can never match a new socket's id.
- **Lock order.** `SOCK_OPTS` is a leaf lock. It is taken under SOCK_TABLES and under a stack lock, and it only nests `vfs::ofd`'s leaf lock.
- **Edge case: ofd 0** (the description table is full). The options are applied to smoltcp but not remembered, so getsockopt reads the defaults.

**Where the options are applied** (`apply_tcp_opts`):
- `connect()`: after `tcp::Socket::connect`, because connect resets the socket's timers.
- `accept()`: the options are copied from the listener's ofd to the new ofd and applied to the established handle. The handle was the listener's own smoltcp socket, which never had the options applied.
- `setsockopt` on a connected TCP socket: applied at once, holding SOCK_TABLES and then the stack lock (the documented order), so the handle cannot be freed underneath.
- `tcp_connect_status`: when it settles a finished handshake, it arms the keepalive timeout. Every send, recv, poll and blocking connect goes through it.

## Semantics (matching Linux)
**TCP_NODELAY (6/1).**
- Maps to `set_nagle_enabled(!nodelay)`. Nagle is on by default.
- getsockopt returns 0 or 1 with optlen 4. A value of 7 reads back as 1.

**SO_KEEPALIVE (1/9).**
- Accepted on any socket (TCP, UDP, AF_UNIX) and read back as 0 or 1.
- It only acts on TCP.

**TCP_KEEPIDLE/INTVL/CNT (4/5/6).**
- Defaults are 7200, 75 and 9.
- Ranges are 1..=32767, 1..=32767 and 1..=127. Anything outside is EINVAL and leaves the old value in place.

**Inheritance.** An accepted socket inherits NODELAY, KEEPALIVE and idle/intvl/cnt from the listener, as Linux's `sk_clone_lock` does. AF_UNIX accept does not inherit, and neither does Linux's `unix_stream_connect`.

**The TCP level on a non-TCP socket.**
- AF_UNIX: EOPNOTSUPP, because unix ops have no protocol setsockopt/getsockopt.
- UDP, ping and raw sockets: ENOPROTOOPT, because `ip_setsockopt` rejects the level.
- This applies to setsockopt and getsockopt alike. Before this change, getsockopt TCP_NODELAY on an AF_UNIX stream socket returned 0.

**Option length.** `optlen < 4` is EINVAL for these options. Other TCP options on a TCP socket are still accepted as no-ops.

## Keepalive approximation
smoltcp has one keep-alive interval and one timeout ("no packet from the peer for this long"). The mapping is:
- **keep-alive interval = keepidle.** The first probe goes out after keepidle seconds of quiet, exactly as on Linux.
- **timeout = keepidle + keepintvl * keepcnt.** A silent peer is dropped at exactly Linux's deadline, counted from the last packet received.

What differs:
- **Probe spacing.** Probes repeat every keepidle seconds, not every keepintvl. With the defaults, one probe goes out before the 7875 s deadline instead of nine, so a single lost probe on a lossy path is fatal where Linux would retry. When keepidle <= keepintvl * keepcnt there are at least two probes.
- **The timeout also covers outstanding data.** smoltcp's timeout covers unacked data as well as idle time, so with keepalive on, a peer that stays silent for the whole deadline while we have data in flight is also dropped. Linux would rely on retransmits (tcp_retries2, about 15 minutes) there. This only matters with tiny test values.
- **No timeout during the handshake.** The timeout is not armed until the handshake completes, because smoltcp's timeout would otherwise also cut a SYN exchange short, which Linux's keepalive does not.
- **No timeout with keepalive off, as before.** smoltcp's timeout would abort a merely idle connection.
- **An immediate first probe.** Enabling keepalive on an idle, established socket sends one probe at once: smoltcp winds the timer up. Linux waits keepidle. This is harmless.

## Nagle vs NODELAY: no measurable difference here
**Measurements.**
- 127.0.0.1 write-write-read ping-pong: about 21.4 ms per round with either setting, on both arches.
- 2 + 40 one-byte writes 2 ms apart to the DNS server: 10 or 11 packets with either setting.

**Cause.**
- The net daemon only transmits on its 100 Hz poll.
- The ACK for the previous segment arrives within the same tick: loopback handles it in one `Interface::poll`, and the vmnet host's RTT is well under 10 ms.
- So Nagle's hold never outlasts a tick, and every tick sends what is buffered either way.

**Where NODELAY would matter:** peers whose RTT or delayed ACK exceeds a tick, for example internet hosts, where Nagle would hold a second small write for one RTT.

**Status of the case.** `tcp_nodelay_latency` therefore only reports its numbers and counts as a SKIP. That NODELAY reaches smoltcp is covered by code and the get/set cases, not by a timing assertion.

## Tests
New nettest cases:
- **`tcp_nodelay`** checks:
  - Default 0.
  - Set on a listener before bind/listen reads 1.
  - Set before connect reads 1, and still reads 1 after connect.
  - The accepted socket inherits NODELAY=1 and SO_KEEPALIVE=1.
  - Clear and set on a connected socket.
  - Data still flows.
  - optlen 2 is EINVAL.
  - UDP gives ENOPROTOOPT and AF_UNIX gives EOPNOTSUPP, for both set and get.
- **`tcp_keepalive`** checks:
  - The defaults 0/7200/75/9.
  - Round trip of 1/30/10/4.
  - KEEPIDLE 0 and KEEPCNT 128 are EINVAL.
  - SO_KEEPALIVE on UDP and AF_UNIX.
  - KEEPIDLE on UDP is ENOPROTOOPT.
  - A 127.0.0.1 connection with idle=1, intvl=1, cnt=2 (3 s deadline) idles 6 s and still carries data both ways. The deadline must not kill a peer that answers its probes.
- **`tcp_keepalive_probes`** checks that a connection to the DNS server (port 53, holding half a DNS message so the server waits) sends at least 3 more NIC packets (`/proc/net/dev`) during 4 s idle with a 1 s keepalive than without one. This proves the probes reach the wire.
- **`tcp_nodelay_latency`** is informational, as described above.

The cases also build and pass against the macOS host kernel (`cc nettest.c`). BSD differences are guarded under `__APPLE__`: a flag reads back as its bit value, the range checks differ, and errnos are any error.

**Before the fix** (old server, new nettest; aarch64, `/tmp/tcpopts/tests-before-a64`):
- `tcp_nodelay: FAIL listener NODELAY reads 0 after set (want 1)`
- `tcp_keepalive: FAIL defaults keepalive/idle/intvl/cnt = 0/-1/-1/-1 (want 0/7200/75/9)`
- `tcp_keepalive_probes` was added after that run. On the old server setsockopt is a no-op, so it would see 0 packets in both runs and fail its ">= 3 more" check.

**After the fix:**
| Run | Arch / accel | Result | Output |
|---|---|---|---|
| `runtests regress` | aarch64 / HVF | 15/15 PASS | `/tmp/tcpopts/tests-regress-a64` |
| `runtests regress` | x86_64 / TCG | 15/15 PASS | `/tmp/tcpopts/tests-regress-x86` |

In both regress runs nettest reported 20 passed, 0 failed, 2 skipped (`tcp_http` needs `-t`; `tcp_nodelay_latency` is informational). Probes: 0 packets without keepalive and 4 with it, on both arches.

**Firefox** (`ffsession --url https://example.com/`): "Example Domain" rendered over https on aarch64 (`/tmp/tcpopts/run-ff-a64/ff-4.png`) and on x86_64 (`/tmp/tcpopts/run-ff-x86/ff-4.png`). NSPR sets TCP_NODELAY on its sockets, so this path now goes through the new code.

## Incidents
- **The Mac data volume ran out of space mid-lane.** It reached 0.5 to 2 GB free, and other lanes' builds were also running.
  - The first baseline build failed in the x86_64 brush compile with ENOSPC.
  - In a later build, `llvm-strip` hit ENOSPC on the **shared** `.claude/worktrees/mame/mame-aarch64` and left a 2.2 KB file. I re-ran `make -f Makefile.leandros ARCH=aarch64` in that tree and it is back to 372 MB.
  - This lane's aarch64 image still carries the truncated mame. That does not matter for the network tests, but the image should not be reused for mame.
  - Free space needs attention. `/Users/forain/code/brush/target` alone is 74 GB, including a 3.4 GB host `debug/` dir last touched 2026-08-10. I did not delete anything there.

## Open
- **Probe spacing.** A faithful version (probe every keepintvl after the first) needs a per-socket "probes outstanding" state that smoltcp does not expose. One way: drive `set_keep_alive` from the daemon's poll loop.
- **TCP_USER_TIMEOUT** is not implemented. It would map onto the same smoltcp timeout, and the two would need to be reconciled.
- **No dead-peer detection test.** There is no way to make a loopback or vmnet peer go silent. The timeout path rests on smoltcp's own `test_established_keep_alive_timeout`.
- **Clearing NODELAY on a connected socket does not flush**, unlike Linux's `tcp_push_pending_frames` on set. This is moot here, because the next poll tick sends anyway.
