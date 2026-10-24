# lane/laptop2 - real-laptop verification of main 7689d859 (2026-10-23)

Result: VERIFIED and PROMOTED. Kernel `#1 laptop2` (id c804a453b17a, main 7689d859 + i8042 202f79cb, irisseg, nvmereset, evgkey) is the laptop's good kernel.
Previous good c2683ee4ddd3 (irisseg-fix) was replaced. Trial boot used netwatch=180; booted in 35 s.

## Health checks (trial boot, klog + greetd.log)
- i8042: probe present, self-test 0x55, kbd port ok; MUX rev 1.1; AUX1 pointing stick, AUX2 Synaptics absolute. `[PSMOUSE] selftest: ok`.
- /dev/input/event0 and event1 present. A real keypress was not tested (needs a human).
- `[PSMOUSE] IRQ 12 -> GSI 12 -> vector 33` is logged. There is NO equivalent log line for IRQ1: ioapic::set_isa_irq(1, ..) in arch/x86_64/src/lib.rs prints nothing.
  Suggest adding one if the routing needs to be checkable from klog.
- NVMe: ready in 14 ms, no reset/timeout/abort lines after 5+ min idle (the "controller disabled" line is the init sequence).
- i915: probe 239 ms, G3b exec ready 18 ms; greetd.log GREETER-REAL renderer=iris (Mesa Intel UHD 620 KBL GT2).
- Wi-Fi: iwlwifi runtime firmware up, wifi0 stack up, SSH over Wi-Fi throughout.
- No panic / WDOG / SMAP violation / BUDDY errors (0 matches).

## Idle census (greeter, 10.3 s window, snap.sh + an.py)
Total 8.0 % of one core: cosmic-comp 2.9, iwlwifi 1.9, cosmic-greeter 1.5, netd 0.9. Top thread: cosmic-comp surface-HDMI-A 2.4 %.
epoll calls 95/s, parks 179/s, epollx useless 110/s - same as the earlier baseline (8.2 %).

## SSH heartbeat (idle, 5 min, 1 Hz, new connection per beat via remote.py run)
299 beats, 299 ok, 0 failures, median gap 1.01 s, max 2.11 s, 1 gap > 2 s.

## Not done (needs the user's password / a human)
Desktop login, keyboard typing test, playback, L2.7/L2.8/L3.x desktop census.
