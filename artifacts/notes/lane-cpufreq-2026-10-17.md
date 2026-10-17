# lane/cpufreq — 2026-10-17

## Problem

The kernel never programmed a P-state MSR (no PERF_CTL / PM_ENABLE / HWP_REQUEST
anywhere in the tree). On the HP EliteBook 840 G5 (i5-8350U, Kaby Lake R,
base 1.7 GHz, turbo 3.6 GHz) `sha256sum` throughput implied ~0.7–1.2 GHz and
COSMIC felt sluggish: the CPU sat at whatever P-state the firmware left.

## What changed

`arch/x86_64/src/cpufreq.rs` (new), called per logical CPU — BSP from
`arch_x86_64::init` after `syscall::init`, each AP from `smp::sched_ap_entry`
before it counts itself online (so boot lines do not interleave).

* **HWP** (CPUID.06H:EAX[7]) — the laptop's path. `IA32_PM_ENABLE`=1,
  read `IA32_HWP_CAPABILITIES`, write `IA32_HWP_REQUEST` = min lowest,
  max highest, desired 0 (autonomous), EPP 0x80. Part without the EPP field
  (EAX[10]) → `IA32_ENERGY_PERF_BIAS` instead. Package request bit 42 left
  clear, so `HWP_REQUEST_PKG` is never written.
* **EIST** otherwise (CPUID.01H:ECX[7]) — `MISC_ENABLE` bit 16 on,
  `PERF_CTL` = `TURBO_RATIO_LIMIT` low byte (fallback `PLATFORM_INFO`[15:8]).
* Both: firmware turbo-disable (`MISC_ENABLE` bit 38) is cleared if it takes
  the write (reported `turbo=on(enabled)` / `off(locked)`).
* Every MSR access is `rdmsr_safe`/`wrmsr_safe`: a #GP at those two
  instructions is caught by `idt::fault_common` (`cpufreq::msr_fixup`) and
  returns failure instead of halting the CPU.
* Knobs: `cpu.pstate=0` (touch nothing), `cpu.hwp=0` (EIST path),
  `cpu.turbo=0` (keep a firmware turbo-disable), `cpu.perf=1` (EPP 0x40),
  `cpu.epp=N` (0..255, wins over cpu.perf).
* Sampling: each CPU's timer tick snapshots TSC/APERF/MPERF/PERF_STATUS at
  most once a second (only when CPUID.06H:ECX[0]). `/proc/cpufreq` shows per
  CPU: mode, caps (lowest/efficient/guaranteed/highest ratios), the request
  written, `eff_mhz` = ΔAPERF/ΔMPERF × TSC MHz, `busy%` = ΔMPERF/ΔTSC (C0
  residency), `status_mhz` = PERF_STATUS ratio × 100, `age_ms` of the sample
  (a CPU in tickless idle keeps an older one).

## QEMU

TCG `-cpu max` has neither HWP, EIST nor APERF/MPERF: every CPU logs
`[cpufreq] cpuN none: no HWP/EIST in CPUID (hypervisor) aperfmperf=no`,
`/proc/cpufreq` shows `none` rows with `-` samples. Regress 24/24 on both
arches (+ `cat /proc/cpufreq`); aarch64's `/proc/cpufreq` says "not
implemented on this architecture".

`-cpu max,+est` does not take under TCG (still "none"), so the HWP/EIST
programming paths are untested until the laptop. The #GP fixup is tested:
`cpu.msrtest=1` (new knob) logs
`msrtest: rd(bogus)=no fault wr(bogus)=no fault rd(0x802)=#GP caught wr(0x803)=#GP caught rd(APIC_BASE)=ok: PASS`
— TCG silently reads unknown MSRs as 0, but faults on the x2APIC range in
xAPIC mode, and both faults landed on the fixup and boot continued.

## On the laptop — what to look for

1. Boot log: 8 lines like
   `[cpufreq] cpu0 HWP lowest=4 efficient=~8 guaranteed=17 highest=36 -> req=0x80002404 epp=128 turbo=on aperfmperf=yes`
   (i5-8350U: ratios 4 / 17 / 36 = 400 MHz / 1.7 GHz / 3.6 GHz).
   `turbo=off(locked)` means the BIOS turbo setting is locked off — check BIOS.
   `WRITE FAILED` or `none: HWP in CPUID but ...` = the firmware locked HWP out;
   retry with `cpu.hwp=0`.
2. `cat /proc/cpufreq` idle, then while `sha256sum` runs: the busy CPU should
   show `eff_mhz` 3000–3600, busy% ≈100, status_mhz ~3400–3600.
3. `time sha256sum /bin/cosmic-comp`: baseline 607 ms → expect ~150–200 ms.
4. A/B: `cpu.pstate=0` reproduces the old behaviour; `cpu.perf=1` / `cpu.epp=0`
   for maximum responsiveness, `cpu.epp=192` to save battery.
5. Watch temperatures/fan: the CPU will now actually turbo.
