# lane evdevvt 2026-11-13 - REFUTED (premises stale)

TODO.md "VT switching" items (c), (d), (e) were already done:
- (d) EVIOCREVOKE: `EvdevState::revoke` in servers/evdev/src/lib.rs (read/ioctl/write ENODEV, poll POLLERR|POLLHUP, grab released, other opens untouched); vttest t_revoke + t_revoke_releases_grab.
- (c) EVIOCGRAB exclusivity: enforced in `broadcast`; docs/input.md records grabber 30 / other 0; vttest grab tests.
- (e) Ctrl+Alt+Esc is the intentional escape hatch (docs/servers/tty-and-vt.md), routed upstream of broadcast; not swallowed by a bug.
- Evidence: aarch64 HVF `/bin/vttest` RC=0 fails=0. `/bin/evsplit` alone is a role helper (needs injected events; STARVED without them), not a standalone test.
Open: (a) present-during-switch flash, (b) evdev VT gate vs cosmic-comp (need desktop session; not attempted).
