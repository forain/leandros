# lane/lapdispB 2026-10-28: HPD interrupt, i915-hpd thread, DRM change uevent (no-monitor half)

Design: artifacts/notes/design-lapdisp2-2026-10-27.md lane B. Everything is behind `i915.extout=1` (off by default).

## What changed
- `vblank.rs`: SDEIMR/SDEIER unmask for DDI B/C/D (bits 21..23) via `hpd_irq_enable`, only when the display MSI is live.
  ISR: on an SDE hotplug bit it reads PCH_PORT_HOTPLUG, writes it back (W1C), hands the status to `outputs::hpd_irq`
  (atomic OR + semaphore up, no locks) and resets `IRQ_IDLE_STREAK` without counting a vblank. `kexec_irq_off` also masks SDE.
- `outputs.rs`: `HPD_PENDING`, kthread `i915-hpd` (event-driven; 2 s semaphore timeout is the SDEISR live-bit backstop),
  `reprobe()` (old `info()` body), `info()` is cache-only once the thread runs, debounce `i915.hpd_debounce_ms` (100),
  `i915.hpd=0` = no IRQ, poll only. Long pulse or live-bit change => forced reprobe; changed (connected, modes) => uevent.
  Short pulse is logged only. 300 s `hpd stats:` line (irqs, wakes, reprobes, PCH_PORT_HOTPLUG, SDEISR live).
- `servers/evdev`: `hp_drm_change(card)` pushes `change card0 drm 0 0 0` on /dev/input/hotplug.
- libudev shim: DRM-only monitors open the hotplug node; the record becomes card0 (devnum 226:0) with action `change`,
  `HOTPLUG=1`; input and DRM records are filtered by the monitor's subsystems.
- ioctl 0x100B `DRM_DEBUG_FAKE_HPD` (root only, kernel/src/syscall.rs whitelist + drm_device_interface.rs).
- `drmsmoke --hotplug [--fake]`: waits <= 60 s for the drm record, then `--list`.

## Verified without a monitor
- x86_64 QEMU `i915.fake=2 i915.kms=1 i915.extout=1`: `drmsmoke --hotplug --fake` PASS (record + connector enum).
- Regress x86_64 / aarch64: see the results block at the end of this note.
- Laptop: see the results block at the end of this note.

## [MONITOR] steps for the user (lane B tests 3-4, bundle with lapdispC2)
1. Boot a trial with `--append i915.extout=1` (never bare `espctl cmdline`). Expect in klog:
   `outputs: HPD irq enabled (SDEIMR 0x........ SDEIER 0x........ ...), kthread i915-hpd`.
2. Run `drmsmoke --hotplug` in a loop over SSH; plug and unplug HDMI 3 times. Each plug must print
   `outputs: hpd DDI-x long pulse ... -> reprobe`, `outputs: HDMI-A-1 hpd=true -> connected (N modes ...)`,
   `outputs: uevent drm change card0 (gen N)`, and drmsmoke exits 0 within 1 s.
3. `drmsmoke --list` x100 while plugged: each should cost < 1 ms (cache only).
4. With cosmic-comp running, set `LIBUDEV_TRACE=1` for it and look for `mon_recv ... action=change syspath=/sys/class/drm/card0`
   and a connector scan in its log on each plug (the connector stays hidden until lane C wires a CRTC).
5. Also try an DP-alt/USB-C adapter: expect long/short pulse lines; short pulse is only logged.
6. If PCH_PORT_HOTPLUG shows no pulse on a plug: collect `outputs: hpd stats:` and SDEISR live bits (live changes are
   caught by the 2 s backstop, line `live bit changed without a pulse`).
