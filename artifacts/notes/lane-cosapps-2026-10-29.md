# lane/cosapps 2026-10-29

cosmic-files, cosmic-edit, cosmic-screenshot (epoch-1.3.0) staged as /bin/<app> + .desktop + hicolor icons
(ports/cosmic-apps, mkfs-f2fs-populated.py block, build-all.sh hook). gvfs and io-uring dropped.

- aarch64 build-all: 0 `^error`, 0 `WARNING:`.
- aarch64 regress: all green except host:wifisimtest.py (fails proc_net_wifi); unrelated to this lane, not investigated.
- COSMIC session (QEMU aarch64, virgl): env from cosmic-panel /proc/PID/environ. panel has no WAYLAND_DISPLAY
  (socket handed over), so apps panic "neither WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is set"
  until WAYLAND_DISPLAY=wayland-1 is exported (from $XDG_RUNTIME_DIR) and WAYLAND_SOCKET unset.
  With that: all three alive after 15 s, no panic. cosmic-files opened /home/leandro (thumbnail cache dir
  ERRORs on first run only), cosmic-edit opened a file, cosmic-screenshot started (wgpu vulkan enumerate
  warning, falls back to GLES). No PNG in ~/Pictures: screenshot was launched without args, so it waits
  for interactive capture; the capture path (portal/screencopy) is NOT verified.
- Print key is bound by cosmic-comp to System(Screenshot).
- x86_64 build-all: see merge commit result.
