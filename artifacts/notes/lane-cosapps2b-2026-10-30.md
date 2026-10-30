# lane/cosapps2b 2026-10-30 (PARTIAL)

cosmic-store: DELIVERED (staged, no usable backend). cosmic-player: BLOCKED (needs GStreamer).

## cosmic-store (epoch-1.3.0)
- Features (upstream flags only, no source patch): `dbus-config,desktop,logind,notify,packagekit,single-instance,wayland,wgpu,xdg-portal`.
  Without any backend feature it does not compile upstream (empty-backend type inference, backend/mod.rs:239), so
  `packagekit` (pure-Rust zbus) stands in; `flatpak` needs libflatpak (GLib/GIO/ostree), absent from the sysroot.
  `desktop` and `logind` are needed to compile (`freedesktop_desktop_entry`, `zbus` uses).
- Link needed the `__stack_chk_guard` shim (aws-lc-sys via reqwest/rustls; sysroot libc.so does not export it):
  build.sh compiles ports/mesa/ssp_guard.c and passes it with `--config target.<t>.rustflags`.
- QEMU aarch64 virgl COSMIC session (.claude/skills/run-leandros/cosstoresession.py, launched with panel env + WAYLAND_DISPLAY=wayland-1):
  alive after 20 s, no panic. Log: "failed to load packagekit backend: No such file" (expected, no PackageKit service),
  wgpu falls back Vulkan -> GLES, opens a Wayland window. Store shows no packages.
- Staged as /bin/cosmic-store + com.system76.CosmicStore.desktop + hicolor icons (mkfs-f2fs-populated.py loop).

## cosmic-player: blocked
Hard deps: gstreamer-tag 0.25, the wash2/iced_video_player fork (gstreamer-app/video), no feature disables them.
ports/ has no glib, gstreamer or ffmpeg (Firefox bundles its own ffmpeg inside the Alpine closure, not reusable
as GStreamer). Missing: glib/gobject/gio, gstreamer core, plugins-base/good, decoder (gst-libav+ffmpeg or -bad),
PipeWire audio sink (gst pipewire plugin or pulsesink via pipewire-pulse), plus dev pkg-config files in
m3-gl-stack/sysroot-aarch64. Est. 80-120 MB, 1-2 days (Alpine 3.21 repackage). Not started.
