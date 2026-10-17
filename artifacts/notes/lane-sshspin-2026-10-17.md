# lane/sshspin — 2026-10-17

## Symptom
Laptop, kernel main-e93d2049: in-guest sshd at ~100% of one CPU while any SSH
session is open, ~0 without one. lockstat: EPOLL ~1.3M/s, FD_TABLE ~1.3M/s,
RUN_QUEUE ~580k/s, PIPE_RINGS ~230k/s.

## Root cause (kernel, net server)
EPOLLET was not honoured for **TCP sockets**. `net::handle_poll` reported an
edge-seq only for connected AF_UNIX sockets; inet sockets answered
`data[16]==0`, which `probe_fd_events_seq_inner` treats as "no edge source,
level-triggered even under EPOLLET". tokio/mio registers every `TcpStream`
`EPOLLIN|EPOLLOUT|EPOLLRDHUP|EPOLLET`; an idle connection is permanently
writable, so every `epoll_wait` in sshd's reactor returned the TCP fd with
EPOLLOUT at once -> a hot reactor loop (and each pass also probes the child's
stdin/stdout/stderr pipes: the PIPE_RINGS traffic). No session = no TCP
connection = no spin. Not a sshd/tokio bug: Linux reports that writable edge
once.

## Fix
`servers/net/src/lib.rs`: per-stack `NetStack::inet_seq` (handle -> seq,
values from a global monotonic `INET_SEQ_GEN`, never u64::MAX). The daemon's
before/after-`Interface::poll` snapshot (`readiness_sigs`/`readiness_moved`)
now also records each TCP socket's recv/send queue lengths; only the network
changes a socket during the poll (stack lock held), so any difference is an
edge (data/FIN arrived, ACK freed send space, state change, reset) -> new seq
+ tag wake. The queue lengths cover the edge the readiness byte misses: more
data arriving on a socket that was already readable (an EPOLLET waiter parked
on it needs both the new seq and the wake). `handle_poll` reports the seq for
connected TCP (data[16]=1). UDP/ICMP/listeners stay level (smoltcp 0.11 UDP
has no queue length to edge on; a tokio UdpSocket would still re-fire on
EPOLLOUT — open, not seen in practice).

## Evidence (QEMU, 10 s windows, `sshspin.py`: sshd /proc/<pid>/stat utime+stime, /proc/lockstat deltas)
| | sshd ticks/s (100 = 1 CPU) | EPOLL/s | FD_TABLE/s | RUN_QUEUE/s | PIPE_RINGS/s |
|---|---|---|---|---|---|
| aarch64 before, idle | 0.0 | 484 | 839 | 3.4k | 59 |
| aarch64 before, session | **100.6** | 1,306,887 | 1,545,040 | 835,509 | 237,628 |
| aarch64 after, session | 0.1 | 530 | 1,199 | 3.7k | 118 |
| x86_64 after, session | 0.4 | 828 | 1,599 | 4.1k | 149 |

Bulk over ssh after the fix (lost-edge check): aarch64 50 MB down 2.2 s,
25 MB up 2.8 s; x86_64 (TCG) 20 MB down 5.6 s, 10 MB up 3.4 s. sshsmoke
--full 19/19 (aarch64).

## Regression test
`epolltest` case 12 `tcp_et_no_refire`: loopback TCP pair, client registered
IN|OUT|ET; one initial EPOLLOUT, then 20 x epoll_wait(10ms) must not re-fire
(<=1 allowed), a write is an IN edge, a second write while unread is another,
then quiet again. Before the fix: `refires=20 after=10` FAIL; after: 0/0 PASS.

## Tests
regress (24 steps) PASS on aarch64 (HVF) and x86_64 (TCG).
