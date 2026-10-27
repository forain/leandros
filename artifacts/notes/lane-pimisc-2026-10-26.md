# lane/pimisc, 2026-10-26: Pi 5 RNG, RTC, watchdog/reset, thermal, cpufreq

Status: DELIVERED (host tests, compile checks, aarch64 regress). Not yet run on a Pi.
Inventory: `artifacts/notes/pi5-device-inventory-2026-10-26.md`.

## What landed
- `drivers/src/rpi_misc_logic.rs` (pure, host-tested): iproc-rng200 sequences, PM watchdog
  arm/kick/disarm/restart/halt, AVS raw → m°C (dts coefficients -550 / 450000), one-tag firmware
  message build/parse, RTC plausibility (≥ 2020-01-01), two-level ondemand governor.
- `drivers/src/rpi_pm.rs`: the one PM-block accessor (lazy ioremap of 0x10_7d200000, lock for RMW).
  `v3d::probe_boot` now takes its PM window from `rpi_pm::base()` instead of mapping its own.
- `drivers/src/rpi_misc.rs` (rpi5 only), called from `kernel/src/init.rs` right after the V3D probe:
  - `[rng]`: rng200 at 0x10_7d208000, 32-byte boot seed via `sched::random::add_boot_seed`, then
    registered as a reseed source (`register_source("rng200")`) → getrandom/`/dev/urandom`.
  - `[RTC] rpi`: firmware GET_RTC_REG(RTC_TIME) → `sched::set_realtime_ns`. clock_settime /
    settimeofday write back with SET_RTC_REG (`kernel/src/syscall.rs step_realtime_ns`).
    `time_init`'s "no battery clock" line is suppressed on rpi5 (the RTC line replaces it).
  - `[wdt]`: opt-in `pi.wdt=<1..15>` arms the PM watchdog, kicked every 100 ms by kthread
    `pi-house`; off by default (a firmware-armed watchdog found running is stopped).
  - `[thermal]`: AVS monitor 0x10_7d542000+0x200, firmware GET_TEMPERATURE fallback.
    API `drivers::thermal::soc_millicelsius() -> Option<i32>` (None off-Pi) for pirp1's fan curve.
  - `[cpufreq] rpi`: ARM clock (id 3) min/cur/max from GET_MIN/GET/GET_MAX_CLOCK_RATE; governor in
    `pi-house` samples `sched::nr_running()` every 100 ms: busy ⇒ max at once, ≤1 busy sample in
    the last 10 ⇒ min. `pi.cpufreq=off` leaves the firmware rate, `pi.cpufreq=max` pins max.
  - All post-boot firmware calls go through `rpi_misc::fw_tag` (one lock; the property buffer is a
    single static). Other lanes calling the mailbox after boot should use it too.
- Reboot/poweroff (`kernel/src/power.rs`): PSCI SYSTEM_RESET/OFF first (TF-A, SMC conduit, already
  present); if it returns, the PM watchdog reset / partition-63 halt + reset (`bcm2835_wdt` style).
- `/proc/thermal` (all arches) and `/proc/cpufreq` on aarch64 (VFS → `arch_thermal_report` /
  `arch_cpufreq_report` in `drivers/src/thermal.rs`). On virt: "thermal: no sensor", "cpufreq: no driver".

## Evidence
- `cd drivers/rpi-misc-tests && cargo test --target aarch64-apple-darwin`: 13/13. The mock first
  failed on the halt test (RSTS 0x20 aliases the rng FIFO offset in a shared mock); fixed in the mock.
- `scripts/check-rpi5.sh` and `CHECK_FEATURE=raspi4b`: clean, no warnings in the new files.
- Mac HVF: `cat /proc/thermal`, `cat /proc/cpufreq`, /bin/sigtest PASS; virt still prints its PL031 `[RTC] epoch seconds`.
- Box aarch64 full regress (TCG, 8121dce8): 49 suites, 0 bad.

## Expected on the next Pi boot
```
[rng] iproc-rng200 ok fifo=<n> (entropy source registered)
[RTC] rpi: epoch seconds: <n>        (or: clock unset (<n> s); set it with date -s)
[wdt] bcm2712-pm ready (off; pi.wdt=<1..15> arms it)
[thermal] soc <45..70>.<d> C (avs)
[cpufreq] rpi arm min=1500 cur=<..> max=2400 MHz governor=ondemand
```
`reboot` should then restart via PSCI ("reboot: Restarting system"); if TF-A refuses, the next line
is "reboot: PSCI SYSTEM_RESET refused; PM watchdog reset".

## Open
- Not hardware-verified. The governor's load signal is instantaneous `nr_running` (no per-CPU idle
  accounting exists; idlestat is compile-time off). A /dev/watchdog userspace interface is not done.
- Uncovered devices worth lanes: see the inventory note (pcie0/NVMe, BCM GPIO+l2-intc, BT, OTP, RP1 ADC).
