# lane/kmodhdmib (H2): HDMI audio as a loadable module

Status: READY FOR PI TRIAL (PARTIAL until the Pi trial confirms probe/playback).

- HDMI_AUDIO is a tristate. modules/hdmi-audio reaches hardware through kenv::hdmi (dma40 kapi, hdmi_sink_info, iounmap, dma_sync, serror_checkpoint).
- drivers/src/hdmi_plat.rs holds the OF compatible, built-in claimer and announce(); the boot probe stays in the kernel. KHdmiSink gained `d0`; NEXPORTS unchanged.
- Kernel-side const asserts (kernel/src/kmod/exports/dma40.rs, aarch64+rpi5) pin the module's PROBE_/PORT_/SINK_ literals to hdmi_probe.
- Evidence: `--rpi5` with CONFIG_MODULES=y CONFIG_HDMI_AUDIO=m links hdmi_audio.kmod, kmodpost + bidcheck pass, modules.alias has `of leandros,pi5-hdmi-audio hdmi_audio`; target/kconfig-aarch64/.config has BOARD_RPI5=y, SDHCI=y, HDMI_AUDIO=m (main d4c8a459 config layering).
- Pi trial build: `printf 'CONFIG_MODULES=y\nCONFIG_HDMI_AUDIO=m\n' > .config; ./scripts/build-all.sh --rpi5; rm .config`.

## Pi 5 trial (2026-10-31, orchestrator)

Trial b42d585ad9 (rpi5 defconfig + MODULES=y HDMI_AUDIO=m; -dirty = the Cargo.lock entry for kmod-hdmi-audio, committed with this note):
- root f2fs mounted, wifi0 reachable; dma40 selftest ok (ch10, 64 KiB, irq mode).
- `[kmod] alias of leandros,pi5-hdmi-audio hdmi_audio: Init(-19)`: module loaded, init saw no sink
  (hpd=0, no monitor on the Pi) and returned -ENODEV, slot freed. Same inert result as H1 (=y).
- kexec trial -> good back after 22 s. Not promoted (=m trial). Audio on a real sink is untested (no monitor).
