# lane/tsccal — x86_64 kernel clocks on the calibrated TSC (2026-09-18)

Machine: linux laptop 172.16.149.179 (i5-8350U, host TSC 1896.002 MHz per Linux's
refined calibration), worktree `~/Projects/leandros-tsccal`. x86_64 iterated under
KVM there; aarch64 verified under TCG there; x86_64/TCG verified on the Mac.

## Root cause

`drivers::snd::monotonic_us()` on x86_64 was `_rdtsc() / 1000` — a raw cycle count
divided by an assumed 1 GHz. Every consumer of it ran on that scale: PipeWire's
`push_spool_blocking` stall detector (its "250 ms" was 56 ms real on a 4.49 GHz
7950X, 132 ms on this 1.9 GHz laptop), the "give up after 3 s" drop, the
`[PW] producer gap` threshold, the `[SND] TX stalled t_ms` stamp, and every `*_us`
field of the virtio-gpu / DRM census (`ctrlq_us`, `ctrlq_lat_us`, `FLIP_US_TOTAL`,
`now_us`). lane/hvfclock (`67c9ba1`) had already given `timer::monotonic_ns()` a
PIT-measured `TSC_PER_TICK`; the µs clock in `drivers` never picked it up
because `drivers` sits below `arch-x86_64` in the crate graph.

Other raw `rdtsc` users audited: `lib.rs::putc` (a bounded UART wait — deliberately
raw before calibration, now calibrated after it), `smp.rs::arch_cpu_id`
(`rdtscp` for `IA32_TSC_AUX`, not a time source, unchanged), `timer.rs` (the
calibration itself). `[WDOG]`'s "~N s" is tick-derived (`WD_SCAN_TICKS`), and the
tick count has sat on the TSC grid since hvfclock — it was never on the raw scale.
`[TIMER] … silent for N ms` is aarch64-only (CNTFRQ-scaled, correct).

## What changed (`ff3dd7e`, `06fb2fc`, + docs commit)

- `arch/x86_64/src/timer.rs`: the TSC frequency is resolved once in `init`:
  1. CPUID hypervisor timing leaf `0x40000010` (EAX = TSC kHz) when the hypervisor
     bit is set and the leaf exists;
  2. CPUID `0x15` with the crystal enumerated (exact);
  3. CPUID `0x15` ratio × `0x16` nominal MHz, with the implied crystal snapped to
     the standard part (19.2/24/25/38.4 MHz) when within 1 % — recovers e.g. the
     real 24 MHz × 79 = 1896 MHz from a "1900 MHz" nominal on Kaby Lake;
  4. PIT channel 2: three 10 ms windows, shortest kept (a window can only read
     long), each bounded at 2^32 cycles so a board without an 8254 does not hang
     the boot.
  A stated value wins when the PIT agrees to 5 %; otherwise the PIT measurement
  wins (it is the one comparing the counter against real time); with neither,
  1 GHz. `TSC_PER_TICK = khz × 10`. Printed once: `[TSC] <MHz> (<source>)[; …]`.
  Exported: `timer::tsc_khz()`, `tsc_per_tick()`, `tsc_source()`, and
  `arch_tsc_khz()` (extern "C").
- `drivers/src/snd.rs`: x86_64 `monotonic_us` = `arch_monotonic_ns() / 1000`.
- `arch/x86_64/src/lib.rs`: `putc`'s transmitter wait is half a tick (5 ms) in
  calibrated cycles once known; `UART_TX_WAIT_CYCLES` only before `timer::init`.
- `servers/vfs/src/lib.rs`: `/proc/cpuinfo` `cpu MHz` is the resolved TSC MHz on
  x86_64 (was a hard-coded `1000.000`; unchanged on other arches).
- `userland/timertest`: new `clock_monotonic_tsc_scale` — reads `cpu MHz` back and
  requires a raw `rdtsc` counted over ~300 ms of `CLOCK_MONOTONIC` to run at that
  frequency within 1 %. Fails on the old kernel (reports 1000 MHz on a 1896 MHz
  TSC). No-op PASS on non-x86_64.
- Docs: TODO item removed, DRMSTAT comment, run-leandros skill note.

## Evidence

x86_64 / KVM (laptop, `-cpu host`):
- `[TSC] 1895.546 MHz (pit)` — 0.024 % from the host's 1896.002. QEMU states no
  CPUID frequency with plain `-cpu host` (its CPUID is synthesised; leaves
  0x15/0x16 read as zeros, no 0x40000010 without `tsc-frequency=`).
- With `LEANDROS_QEMU_EXTRA="-cpu host,tsc-frequency=1896002000"`:
  `[TSC] 1896.002 MHz (cpuid 0x40000010); pit measured 1895.921 MHz` — the stated
  path and the cross-check both exercised.
- `timertest` 11/11 PASS (×4 runs; `cpuinfo_tsc_khz=1895546 measured_tsc_khz=1895546`,
  and 1896002/1896003 with the stated frequency).
- `clockdrift.py 30`: err −0.01 %, +0.01 %, +0.11 % (pit), −0.08 %, +0.06 % (stated).
  10 s windows read −0.47 % — that is ±50 ms of serial/echo offset, not rate; use 30 s.
- `/proc/cpuinfo`: `cpu MHz : 1895.546`.
- MAME 8 s run: no `[SND] TX stalled` / `[PW] producer gap` lines at all (healthy
  audio under KVM), so the stamp itself was not observed; it is by construction
  `arch_monotonic_ns/1000`, the clock timertest and clockdrift measure.

x86_64 / TCG (Mac): `[TSC] 999.200 MHz (pit)` (TCG's ns-based virtual TSC),
`clockdrift.py 30` err +0.09 %, timertest 10/10 (main's binary: the image was built
from main's initrd/data images, so it predates the new test).

aarch64 / TCG (laptop): boots to login, timertest 11/11 (new test reports n/a),
`/proc/cpuinfo` still `1000.000`. `clockdrift.py 20` read −1.36 % — aarch64 timer
code is untouched by this lane; either TCG-on-laptop harness offset (20 s window)
or a pre-existing aarch64/TCG rate error worth a 60 s re-measure by whoever owns it.

## Refuted / observed

- "will be ~1.7–4× off on this i5": the raw scale was 1.896× (TSC = 1896 MHz), now 1.0.
- `[WDOG]` timestamps were never on the raw scale (tick-derived) — nothing to fix there.
- One `timerfd_relative_unchanged: FAIL` in 5 KVM runs (elapsed outside 300–400 ms);
  3 immediate re-runs PASS at 304–315 ms. That test rides tick-derived deadlines
  the sibling `timespec` lane is rewriting; not caused here.
- Behaviour change to watch: the audio stall detector on x86_64 now waits a real
  250 ms (it was 56 ms on the 7950X). `audio-glitch-test.sh` on the desktop is the
  check; the threshold was designed and validated on aarch64/HVF where the clock
  was already right.

## Left open

- KVM pvclock (`MSR_KVM_SYSTEM_TIME_NEW`) would give the host's exact `tsc_khz`
  without a PIT window; not needed at 0.02 % but the natural next source.
- Bare-metal x86_64 (deploy-x86_64.sh) is where the `0x15`/`0x16` paths run for
  real; unverified here (no hardware in the lane).
