# lane/netdwake — 2026-10-24 (L9.9 follow-up: net daemon wakes at idle on the laptop)

Status: FIXED (idle target met), NOT promoted (pull throughput A/B inconclusive, see below).

## Attribution
New always-on `/proc/net/nic` line `netd_kick: file:line=count@last_pid ...` (effective kicks only,
`#[track_caller]` on `netdev::kick`/`kick_from_irq`). Measured with the ssh session quiet: a guest
background sampler (`samp.sh`: sleep 15, two 20 s windows, each bracketed by /proc/net/nic + per-task
stat snapshot) started over ssh, ssh disconnected, results pulled 75 s later.

- The "~48 passes/s idle" of wifiirq3 was the measuring ssh session itself. Quiet, good kernel
  c703e3a80dfb: 9.7 and 11.4 passes/s.
- Of the remaining kicks, ~5/s came from `NET_CLOSE_ALL` in the dispatcher (lib.rs:3623): every
  process exit kicked the daemon, socket or not; the sampler's ~100 `cat`s per snapshot showed up as
  ~100 kicks per window (last pid = the short-lived cat/sh). The rest: iwlwifi ISR (348) ~2.5/s,
  iwlwifi RX thread hand-off (1099) ~2/s, TCP send (5002) ~0.2/s, timer-limited sleeps ~1.1/s,
  early wakes ~0. No netctl/NetworkManager polling, no smoltcp timer storm at idle.

## Fix
`handle_close_all` returns whether the process held an inet socket; only then does CLOSE_ALL kick.

## Evidence
- Laptop idle (quiet session), passes/s: good 9.7 / 11.4 -> trial 4.8 / 6.5 (boot 1), 5.3 / 5.2 (boot 1,
  after scp + heartbeat). netd_kick during windows: dispatcher site 0, only iwlwifi IRQ/RX remain.
  Target <= 20/s met; what is left is one pass per received Wi-Fi frame (essential).
- Health (trial): heartbeat 300/300, 0 gaps > 2 s, max 1.16 s (twice); idle CPU 4.9–5.2 % (good 5.1–5.4 %);
  no storm. scp push 24.0 / 25.0 Mbit/s (good today 24.3). scp pull 9.0 / 8.2 Mbit/s vs good 10.3 today
  (single samples; wifiirq3 measured 13.7–14.9 on another day, so today's link is slower for both).
  The change cannot touch the scp path (sshd holds inet sockets and still kicks), but pull was not
  shown >= good, so the trial was NOT promoted; good stays c703e3a80dfb.
- QEMU aarch64: netidletest new `exits` case: unfixed 201 kicks / 200 socketless exits (FAIL),
  fixed 0 (PASS); idle 1.0 passes/s. Full regress (incl. wifisimtest) PASS. x86_64 build OK.

## Open
- Promote after a repeated pull A/B (>= 3 pulls each kernel, same session) shows no regression.
- During scp the daemon runs ~1.5–2 k passes/s, mostly timer-limited sleeps (smoltcp delayed-ACK/RTO
  granularity) on both kernels: worth a separate lane (CPU during transfers, not idle).
