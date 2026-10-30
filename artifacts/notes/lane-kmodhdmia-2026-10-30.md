# lane kmodhdmia (kmodpi H1), 2026-10-30: READY FOR PI TRIAL (=y)

Branch lane/kmodhdmia (off main 81d31ad1), not merged.

## What changed
- Real `hdmi-audio` feature: drivers/Cargo.toml `hdmi-audio`, kernel `drv-hdmi-audio = ["drivers/hdmi-audio"]` (CONFIG_HDMI_AUDIO).
  drivers/src/hdmi_audio/mod.rs: `hw` is compiled only with aarch64 + rpi5 + hdmi-audio, else the old stub.
  `hdmi_audio::sink_info()` (probe facts, feature independent) added.
- dma40 handle table: drivers/src/bcm2712_dma40/handles.rs (6 slots, handle = slot+1; stubs return -19 off rpi5).
- Exports (exports/dma40.rs): kapi_dma40_{claim,release,start_cyclic,stop,position,periods,poll,polled} and
  kapi_hdmi_sink_info (KHdmiSink in kmod-abi plat.rs, sys_dma40 externs). Wrappers: lib/kmod-kapi/src/dma40.rs.
  NEXPORTS 132 -> 141 (docs/kernel/modules.md updated). Rebase over lane kmodmacba: `git merge main` and recount.
- Built-in HDMI now goes through the KSndOps adapter: servers/pipewire/src/hdmi_ops.rs; `Backend::Hdmi` removed,
  pipewire `init` stores `Backend::Module{ops,ctx}`. Only visible difference: shutdown text is "sound module: hdmi audio: nothing to do".
- Not done: no `leandros,pi5-hdmi-audio` announce (nothing registers for it while =y; H2 adds it with the module).
- dma40 handles have no per-owner cleanup (modules are permanent).

## Evidence (aarch64 only)
- ktest=kmodkapi PASS, 141 exports, all new dma40/hdmi_sink_info cases ok.
- Builds ok: aarch64; --rpi5 (llvm-nm: 8 `hdmi_audio::hw` symbols); --rpi5 with CONFIG_HDMI_AUDIO unset (0 `hdmi_audio::hw`).
