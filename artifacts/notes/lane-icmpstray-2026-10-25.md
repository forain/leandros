# Lane icmpstray (2026-10-25) — FIXED (environmental, test made robust)

openbugs L12 #52: aarch64 `/bin/nettest` icmp_poll_timeout + icmp_recv_eintr fail (an ICMP packet arrives).

## Root cause: host environment, not a regression
- Diagnostic run (unfixed test, extra decode): the packet is `type 0 code 0 from 192.0.2.1 id 35 seq 187`
  (sent: id 35 seq 7), 40 bytes, ~0.5-0.8 s after the send.
- A host process `ping -i 1 -c 1100 172.16.255.255` (orphan, ppid 1, started 03:01:31 local; owner unknown,
  not this lane) was running. Test ran at 03:04:38-39 -> host ping seq ~187. Exact match.
- Mechanism: QEMU user-mode net (libslirp) on macOS uses unprivileged SOCK_DGRAM ICMP sockets; macOS
  delivers the host's echo replies (here, broadcast replies) to that socket, and libslirp writes the
  original guest id back into the reply and uses the request's destination as the source. The guest sees a
  reply from 192.0.2.1 with its own id. Host `ping 192.0.2.1` itself gets 0/10 replies.
- Linux demuxes ping sockets by id only, so delivering this to the guest socket is correct; our stack is fine.
  Start time (~02:34 UTC) = when host broadcast pings began, not a merge (lanes blkdead/scanfrag share no code).

## Fix (ff5735e9)
- Both cases drop echo replies whose seq != 7 and keep waiting (ping(8) semantics); they print the stray count.
  A genuine seq-7 reply still fails.
- Red: icmps1/icmps3 (unfixed) failed both cases while the host ping ran. Green: icmpg1-3, 3/3 PASS with
  "ignored 1 stray echo replies" logged each time, host ping still running. Full aarch64 regress: PASS.

## Open
- Whoever starts host-side broadcast pings should stop them after use (also skews other ICMP measurements).
