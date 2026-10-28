# lane/pitcp 2026-10-28: Pi 5 TCP bulk send (FIXED)

Design: artifacts/notes/design-pitcp-2026-10-28.md. Diagnosis H1 confirmed: the vendored
smoltcp 0.11 had no congestion window, so every eth0 burst was a full 256 KiB window.

## Changes
- servers/net/vendor/smoltcp tcp.rs: Reno and CUBIC (`Cc`), `sender_snapshot()`, per-socket
  counters; window = min(remote window, cwnd). Knobs `tcp.cc=none|reno|cubic`, `tcp.iw`,
  `tcp.rto_min_ms`, `tcp.sndbuf_kb`, `net.cadence_us` (read once in netd).
- /proc/net/tcpsnd (one line per sending TCP socket) and `netd_pass_hist:` (frames sent and
  received per pass) in /proc/net/nic.
- Default is `tcp.cc=cubic` (measured results below); `tcp.cc=none` restores the old behaviour.
- Host tests: `cargo test --lib socket::tcp` (crates.io Cargo.toml minus examples) 162/162,
  including three new cc tests.

## Pi 5 (good 902a07c4 base, trial 3bc3d35e; Pi -> Mac, 20 MB, `head -c` over ssh)
| cc | eth0 run1/2/3 (MB/s) | wifi0 |
|---|---|---|
| none | 0.28 stall (13.4 MB in 48 s), 0.28 stall (13.8 MB), 0.58 ok (35 s) | 1.25 ok |
| reno | 25.5, 26.8, 21.1 | 1.52 ok |
| cubic | 28.6, 29.3, 29.4 | 1.55 ok |
(Earlier partial run: reno 17.2/26.5/27.1.) wifi0 stays ~1.5 MB/s: air/cipher-limited, not CC.

## Snapshot at an eth0 stall (cc=none)
flight 262144 (the whole send buffer), snd_wnd 4 MB, timer=retransmit with delay/rto 1876 ms
(inflated by backoff), recovery point set for seconds, counters over a 20 MB run:
fast_retransmit 3143, rto 29, partial_ack 2982, dup_acks 8898. netd_pass_hist tx: 81563 passes
with 0 frames, 7220 with 1, 3064 with 2-7. With reno/cubic the flight stays at cwnd (40-50 KB
in the samples), fast_retransmit 21-37, rto 0-4 per run.

## QEMU aarch64 (slirp, irq-driven NIC)
Did not reproduce: 40 MB guest -> host in ~1 s, so the cap needs line-rate bursts into a
real bridge. Regress aarch64 (box): 57 suites, 0 bad with cc=none; with the cubic default also 57 suites, 0 bad (nettest, udptest, net* included).

## Notes
- After a trial -> trial kexec the Pi's wifi0 does not come back (eth0 does); good -> trial
  keeps both. The matrix therefore booted good between configs (open, not investigated).
- The Pi was left on good 902a07c4; not promoted.
