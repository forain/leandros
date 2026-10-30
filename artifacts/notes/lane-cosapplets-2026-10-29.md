# lane/cosapplets 2026-10-29

Added `ports/cosmic-applets-panel` (build.sh + 8 upstream .desktop files) and a build-all/mkfs hook
that stages each binary together with its .desktop, or neither (with a WARNING).

Applets: time (real one replaces the stand-in .desktop), battery, notifications, status-area,
workspaces, input-sources, a11y, cosmic-app-list.

## Verification (aarch64, QEMU/TCG, --virgl session)
- build-all aarch64: 0 `^error`, 0 mkfs `WARNING:`.
- casession: panel launched time, battery, notifications, status-area, input-sources, a11y,
  app-list, each started once and stayed up; no applet panic in the session log.
- Only panic in the log is cosmic-osd (khronos-egl), not an applet, not from this lane.
- workspaces: binary and .desktop are staged, but the panel's default config does not launch it
  (no "Starting: cosmic-applet-workspaces"). Needs a panel config entry; not faked here.
- battery: starts and stays up with no upower hardware; leandros-sysbus serves a stub
  `org.freedesktop.UPower` name, no real battery data. bluetooth: starts, no bluez service, so
  it shows no adapter. No services were faked.

## Gaps
- Real upower and bluez services do not exist yet (battery and bluetooth show empty state).
- Bluetooth applet has been on main since 6ec4a199, but the laptop root image predates it.
- workspaces applet not in default panel config (see above).
