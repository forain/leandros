# lane/kmodhdmib (H2): HDMI audio as a loadable module

Status: READY FOR PI TRIAL (PARTIAL until the Pi trial confirms probe/playback).

- HDMI_AUDIO is a tristate. modules/hdmi-audio reaches hardware through kenv::hdmi (dma40 kapi, hdmi_sink_info, iounmap, dma_sync, serror_checkpoint).
- drivers/src/hdmi_plat.rs holds the OF compatible, built-in claimer and announce(); the boot probe stays in the kernel. KHdmiSink gained `d0`; NEXPORTS unchanged.
- Kernel-side const asserts (kernel/src/kmod/exports/dma40.rs, aarch64+rpi5) pin the module's PROBE_/PORT_/SINK_ literals to hdmi_probe.
- Evidence: `--rpi5` with CONFIG_MODULES=y CONFIG_HDMI_AUDIO=m links hdmi_audio.kmod, kmodpost + bidcheck pass, modules.alias has `of leandros,pi5-hdmi-audio hdmi_audio`; target/kconfig-aarch64/.config has BOARD_RPI5=y, SDHCI=y, HDMI_AUDIO=m (main d4c8a459 config layering).
- Pi trial build: `printf 'CONFIG_MODULES=y\nCONFIG_HDMI_AUDIO=m\n' > .config; ./scripts/build-all.sh --rpi5; rm .config`.
