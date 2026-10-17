# lane/gtfreq — 2026-10-17: GT frequency (software RPS) for the KBL laptop

## Problem
Boot log on the EliteBook (UHD 620): `RP_STATE_CAP=0x00060616 -> RP0 1100 MHz
RP1 300 MHz RPn 300 MHz; RPSTAT1=0x03000000 -> current 100 MHz`. Nothing wrote
GEN6_RPNSWREQ / RP_CONTROL / RP limits, so the GT ran at ~100-300 MHz however
busy it was: COSMIC on iris sluggish (cursor / window drag).

## What changed
- `drivers/src/i915/rps.rs` (new): at the end of the G3b exec bring-up
  (forcewake held for good) Linux's gen9 sequence: `gen9_rps_enable`
  (RC_VIDEO_FREQ = RP1, RP_IDLE_HYSTERSIS = 10), `rps_reset` -> `rps_set(min)`
  (RPNSWREQ = RPn << 23, LOW_POWER EI/thresholds, RP_CONTROL =
  MEDIA_TURBO|HW_NORMAL|IS_GFX|ENABLE|UP_BUSY_AVG|DOWN_IDLE_AVG = 0xd92,
  RP_INTERRUPT_LIMITS). PM interrupts masked (PMINTRMSK = ~0): no IRQ path.
  Then RP0 is requested and CAGF read after 1 ms each way, logged.
- Software governor `hw::RpsGov` (host-tested): RP0 on the first submit after
  idle (exec::submit), RPn after `i915.rps_idle_ms` (100) with nothing queued or
  in flight (exec poll thread, 5 ms idle period).
- Knobs: `i915.rps=0|off` registers untouched (old behaviour, A/B baseline),
  `i915.rps=max` fixed RP0, `i915.rps=min`, `i915.rps=<MHz>` fixed (clamped to
  [RPn, RP0], 16.67 MHz steps), default/`auto` = governor.
- Instrumentation: `[i915] gt:` line every `i915.stat_s` (10) s from the exec
  poll thread, only when there were exec requests or flips in the window;
  `/proc/i915` (counters since boot, `name value`; ENOENT on QEMU / without exec).
  Flip breakdown = one `rps::flip_stat` call in `kms::present` (no logic change).
- Only touched on the real part: `exec::init` skips it for `i915.fake`, and
  QEMU (virtio-gpu) never runs i915 at all.

## What to look for on the laptop
1. Boot: `[i915] rps: as found: RP_CONTROL=... RPNSWREQ=... CAGF ...` then
   `[i915] rps: enabled ... request RPn 300 MHz -> CAGF N MHz after 1 ms, RP0
   1100 MHz -> CAGF M MHz after 1 ms; mode auto`. Expect N ~300 and M ~1100
   (M may be lower if the PCU caps it: power/thermal limit, or the request needs
   more than 1 ms — the `gt:` line's CAGF is the real answer).
   If M stays ~100: the PCU ignores RPNSWREQ — check RC6/`RP_CONTROL` readback
   in that line; compare with `i915.rps=max`.
2. During COSMIC use: `[i915] gt: cagf 1100 MHz (avg while busy X MHz) req 1100
   MHz (auto, +u up/+d down) | exec rcs R/s bcs B/s, busy rcs A bcs C ms/s |
   flips F/s: fence a/b clflush c/d latch e/f us avg/max, not latched n`.
   - avg-while-busy should be near 1100 (was ~100-300 implicitly).
   - busy rcs ms/s close to 1000 = GPU-bound; low = the bottleneck is elsewhere
     (CPU, present path). fence avg = time present waits for rendering; latch
     avg ~ up to one frame (16.7 ms at 60 Hz) is the vblank wait.
   - up/down counts per 10 s: many dozens means the 100 ms idle drop is
     ping-ponging; try `i915.rps_idle_ms=300` or `i915.rps=max` for A/B.
3. `cat /proc/i915` over SSH: `cagf_mhz`, `req_mhz`, `rpnswreq` (0x21000000 at
   RP0), `rp_control` (0x00000d92), `cagf_busy_avg_mhz`, exec/busy/flip totals.
4. A/B: `i915.rps=0` should reproduce the old sluggishness; `i915.rps=max`
   is the ceiling the governor should match while dragging windows.
5. Watch temperature / fan and battery drain: idle should sit at RPn
   (`req 300 MHz` in /proc/i915 after 100 ms idle).

## Tests
- Host: `drivers/i915-tests` `rps_tests` (units, RPNSWREQ encoding, limits,
  RP_CONTROL, PM intervals, knob parsing, governor state machine) — 51/51.
- QEMU regress both arches (no i915 there; confirms nothing else moved).
