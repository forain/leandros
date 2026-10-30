# lane/cosapps2a 2026-10-30

DELIVERED: cosmic-wallpapers, cosmic-monitor, cosmic-initial-setup (epoch-1.3.0, unmodified).

- Wallpapers: ports/cosmic-wallpapers/build.sh. Upstream checkout holds git-lfs pointers, so originals are fetched
  from media.githubusercontent.com at the pinned commit, downscaled with sips to 1280x720 q80 (same as the Orion default,
  which is untouched). Staged through the shared tree m6-session-data/shared/usr/share/backgrounds/cosmic. +6 images, 1.15 MB.
- cosmic-monitor / cosmic-initial-setup: added to ports/cosmic-apps/build.sh (monitor without nvml). Binaries 48 MB / 55 MB (aarch64,
  same class as cosmic-files 66 MB). .desktop staged; Initial Setup NoDisplay, Autostart entry NOT staged.
- OS fix: /proc/stat was fake (all idle, 0 busy). Now real per-CPU jiffies from an always-on idle counter in sched (hlt/wfi time).
  Measured over 20 s: cpu busy 1060 -> 1345, idle 8740 -> 16459 (4 CPUs).
- Verification (aarch64 QEMU virgl session, ffsession --nofirefox, launched from root shell with the session's wayland/DBUS env):
  both alive after 25 s, 0 panics. Without DBUS_SYSTEM_BUS_ADDRESS initial-setup's network pages panic (zbus system() unwrap); with it none.
  Rendered pages not screenshot-checked.
- Open: /proc/diskstats, /sys/class/net, /sys/class/drm, /sys/devices/system/cpu missing -> monitor disk/net/GPU/freq graphs empty.
