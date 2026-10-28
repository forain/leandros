# lane lapdispA (2026-10-27) - i915 external outputs (extout)

Branch lane/lapdispA, review fixes e0c4f88e, merged with main a6562aa2.
Default: `i915.extout` stays OFF (`== Some(1)` in drivers/src/i915/outputs.rs init()). Flip waits for laptop proof.

## Verified (Mac)
- x86_64 INTEL=1 and aarch64 release builds: no `^error`.
- x86_64 QEMU `i915.fake=2 i915.kms=1 i915.extout=1` drmsmoke: only the 3 known fake failures
  (GET_CAP_SYNCOBJ_TIMELINE_IS_ZERO, SYNCOBJ_TIMELINE_WAIT_ENOSYS, V3D_VERSION_NAME_IS_EXACTLY_V3D); CONNECTOR_ENUM passes.
- x86_64 default boot (headless, no GPU on Mac): boots, /dev/dri present. COSMIC not run (needs a GPU path).
- aarch64 boot: /dev/dri present, hello OK.
- NOT done: --rpi5 build; laptop (172.16.149.179 did not answer ping, skipped, no waiting).

## [MONITOR] steps for the user
1. Laptop: deploy an INTEL=1 trial with per-boot `i915.extout=1` (laptop-deploy-kernel skill, --append, never bare espctl cmdline);
   check cosmic-greeter + cosmic-comp come up and a leandro login works; `drmsmoke --list`; boot `good`; do not promote until proven.
2. Design tests 3-5: HDMI at boot, USB-C DP (which DDI each jack uses), `extout_show=1` without login.
3. Then flip the default to extout ON, rebuild, re-verify, boot good.
Review points to check on hardware: PCH_PORT_HOTPLUG 0xC4030 == hw.rs SHOTPLUG_CTL; enable bits 4/12/20; W1C masks 3<<0,8,16,24; cdclk decode (Linux skl_get_cdclk encoding).
