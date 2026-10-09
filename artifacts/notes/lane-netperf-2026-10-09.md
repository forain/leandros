# lane/netperf — 2026-10-09 — network throughput for YouTube on the real PC

Branch `lane/netperf`, base main `a8573d8`. Not pushed, not merged.

## Root cause

The real PC (`nvrm-boot8.log`, RTL8168h, 1000 Mbps link) downloaded
`speedtest.tele2.net/10MB.zip` at **2.0 Mbit/s**: 4464 frames in 24.74 s,
`rx_missed 0`, no RX errors, `tx_ring_full 0`. Nothing was dropped and
nothing was CPU bound: the sender simply sent ~44 frames (64 KiB) per round
trip, and the round trip to Sweden from this LAN is ~270 ms (the guest's
connect time and the Mac's own `curl` both show it). 64 KiB / 0.25 s =
2.0 Mbit/s. **Every TCP connection was capped at one 64 KiB window per round
trip** (`TCP_RX_BUF = 64 KiB`).

The 100 Hz polled data path made it worse wherever the round trip is short:
a frame waited up to 10 ms for the next daemon pass, the ACK another 10 ms
(smoltcp's delayed-ACK timer ran on `ticks() * 10 ms` timestamps), and a
`send()` waited for the next tick before its segment left. The guest's
"RTT" to a LAN host was ~20-25 ms (ping of the gateway 12 ms on the PC,
19.5 ms in QEMU), so even a host on the same machine got 64 KiB / 25 ms
≈ 21 Mbit/s. virtio-net also posted only 16 RX buffers (24 KB per pass).

Other limits found on the way: UDP sockets had 8 or 16 receive slots
(QUIC — YouTube over HTTP/3 — arrives in trains of tens of datagrams), and
the daemon's readiness publication was a system-wide `wake_poll()` herd per
pass, which could not simply be run 10x more often.

The internet line itself (Mac on the same LAN, 192.168.8.1): ~14 Mbit/s for
one `curl` stream from cachefly (CDN, ~35 ms), ~16 Mbit/s with four, ~3.8
Mbit/s from tele2 (path-limited). So on the PC the expected ceiling is the
line, roughly 15-30 Mbit/s depending on the moment, not 1 Gbit/s.

## Fix

| piece | file |
|---|---|
| TCP sockets: 2 MiB receive window (window scale 6, negotiated by smoltcp from the capacity), 256 KiB send buffer; small sizes past 48 TCP sockets per stack; fallible allocation falling back by halves | `servers/net/src/lib.rs` (`new_tcp_socket`, `alloc_net_buf`) |
| UDP sockets: 512 slots / 512 KiB receive, 128 / 128 KiB send | `new_udp_socket` |
| smoltcp `assembler-max-segment-count-32` (out-of-order holes kept with large windows; was 4) | `servers/net/Cargo.toml` |
| stack clock in microseconds (`monotonic_ns`), not `ticks() * 10 ms` | `stack_now` |
| daemon sleeps until smoltcp's next timer (`poll_delay`), capped at 1 ms while frames flow (200 ms after the last one) and 10 ms idle | `net_daemon` |
| kicks: `drivers::netdev::kick()` on TCP/UDP/ICMP send, on a `recv` that drains a more-than-half-full buffer, and from the r8169 RX interrupt (`kick_from_irq`); only the first kick per pass pays the wake | `drivers/src/netdev.rs` |
| per-socket readiness wakes: `handle_poll` reports `inet_tag(lo, handle)` for connected inet sockets; the daemon snapshots each socket's readiness right before a poll (same stack lock) and wakes the tags of those that moved; a broadcast at most every 100 ms while active stays as a safety net | `readiness_sigs` / `readiness_moved` |
| ingress in runs of 64 frames so ACKs go out during a burst | `RX_BATCH` |
| r8169: MSI RxOK, NAPI style (ISR masks RxOK + kicks; daemon drains, `rx_rearm` acks + unmasks + re-checks the ring); `r8169.rxirq=0` turns it off | `drivers/src/r8169.rs` |
| r8169: RX stall watchdog — no frame delivered for 2 s while overflow interrupts or the `rx_missed` tally rise → chip reset (`resets: N (rx stall M)`) | `driver_thread` |
| virtio-net: 256 RX buffers (was 16), RX doorbell batched (every 32 frames / when the ring is drained) | `drivers/src/virtio_net.rs` |
| `/proc/net/nic`: `netd:` line (passes, max frames per pass, tagged/broadcast wakes, rx_irq on/off, TCP window); r8169 `irq_count: N (rx N ...)` | `servers/net/src/procnet.rs` |
| netcheck: `download` = single stream from a CDN first (cachefly, then tele2/ovh/thinkbroadband) with per-second Mbit/s, then a far server (tele2), then `download4` = 4 parallel streams; whole part < 60 s (≈ 35 s measured). `netcheck --get http://host[:port]/path [--par N] [--secs S] [--mb M]` for manual runs | `userland/netcheck/netcheck.c` |

Not done: TX interrupts (TX completions are reaped on send/poll; not needed
for wakeups), checksum offload / TSO, virtio-net interrupts (QEMU only; the
1 kHz active polling covers it), SACK (smoltcp 0.11 has none).

## Measurements (QEMU on the Mac, same tool before and after)

Before = `a8573d8` kernel with the new netcheck. `--get` single stream
unless noted, 8-10 s each. vmnet = guest TCP end to end through the Mac's
socket_vmnet (gateway 192.168.105.1, host HTTP server on it);
python `http.server` on the Mac serving a 2 GiB file.

| aarch64 (HVF), vmnet | before | after |
|---|---|---|
| host HTTP, 1 stream | 21.6 Mbit/s | **2288 Mbit/s** (1 GiB in 3.75 s) |
| host HTTP, 4 streams | 72.9 | **2786** |
| cachefly (CDN, ~35-45 ms), 1 stream | 8.0 | **31.3** (peak 35.5; line-limited) |
| tele2 (~270 ms), 1 stream, 10 s incl. slow start | 1.0 | **3.2-3.8** (Mac's own curl: 3.8) |
| ping gateway | — | **1.5 ms** |
| Firefox `fetch()` of the host file (virgl desktop) | — | **206.9 Mbit/s** (peak 250.6) |

| x86_64 (TCG), vmnet | before | after |
|---|---|---|
| host HTTP, 1 stream | 19.5 | **514** |
| host HTTP, 4 streams | 73.9 | **493** (CPU-bound TCG) |
| cachefly, 1 stream | 6.7 | **11.7** (peak 22) |
| tele2, 1 stream | 1.6 | **4.2** |
| ping gateway | 19.5 ms | **3.0 ms** |

(SLIRP, before, for reference: host 39.8 / 4 streams 134.8 / cachefly 16.8 —
SLIRP terminates TCP in the Mac, so the guest window never reaches the WAN.)

`netd:` after the aarch64 run: 71921 passes, up to 5803 frames in one pass,
3018 tagged vs 283 broadcast wakes, TCP window 2097152.

Firefox (aarch64, virgl, `ffsession.py`): YouTube
`watch?v=aqz-KE-bpKQ` loads, and with autoplay allowed the video plays
(screenshots at +30/60/90/120 s show it advancing).

## Verification

- `./scripts/build-all.sh`: OK.
- regress: aarch64 **15/15**, x86_64 **15/15** (nettest included).
- `nvidia-test-stick.sh --image ... --quick` booted in QEMU (`--usb-stick --no-gpu`):
  `NETCHECK summary: 9 pass, 0 fail, 0 skip (34.4 s)` — download 16.3
  Mbit/s from cachefly (peak 31.7), far 3.5 Mbit/s, download4 15.3 Mbit/s,
  ping 2.2 ms.
- The r8169 RX interrupt, `rx_rearm` and the RX watchdog are compile-tested
  only (QEMU has no RTL8168 model). If the interrupt never fires, the 1 kHz
  active polling still applies.

## On the PC

Same stick procedure as lane/r8169 (`net.test=1` default). Expected:

```
[r8169] interrupt: MSI vector 0x41 (RX + link/errors)
NETCHECK ping: PASS gateway 192.168.8.1: 4/4 replies, avg ~1 ms        (was 12.1 ms)
NETCHECK download: PASS ... from cachefly.cachefly.net ... = 15-30 Mbit/s   (was 2.0 from tele2)
  download| far speedtest.tele2.net: ... 3-5 Mbit/s                      (path-limited, like the Mac)
NETCHECK download4: PASS 4 streams ... ~15-30 Mbit/s
  nic| irq_count: N (rx M ...)      M grows with traffic: RX interrupts work
  nic| netd: ... rx_irq on tcp_rx_buf 2097152
```

Decision tree:
- `irq_count ... rx 0` after the download: RxOK never fired; throughput still
  comes from the 1 kHz polling. Report it; compare `r8169.rxirq=0`.
- `resets: N (rx stall M)` with M > 0: the RX watchdog fired; send the
  `RX stall:` line (it carries a register dump).
- download well below the line with `hw_tally rx_missed` rising: the ring
  overflowed (daemon too slow) — send `/proc/net/nic`.
- Throughput equal to what the Mac gets on the same LAN is the ceiling;
  YouTube 1080p needs ~5-8 Mbit/s.
