# ports/cosmic-idle: a screensaver inhibitor must not wake a blank screen

One patch on the cosmic-idle tree in `leandros-artifacts/m6-session-bins/src/cosmic-idle`
(0.1.0, built featureless). It fixes an upstream bug: upstream master has the same code. It was
made in lane blankplay (2026-10-18), whose brief allows COSMIC patches for upstream bugs only.

## The bug
cosmic-idle powers the outputs off after `screen_off_time` without input: it fades to black,
then sets every `zwlr_output_power_v1` to Off. It also serves
`org.freedesktop.ScreenSaver.Inhibit`. An inhibitor removes the screen-off timeout, so
`recreate_notifications()` replaces the idle notification and calls
`update_screen_off_idle(false)`, which sets every output back **On**.

Firefox takes that inhibitor (`FreeDesktopScreensaver`, the `LinuxWakeLock` log shows it) when a
video starts to play. So starting a video while the screen is blank lit the panel and kept it lit
for the whole video. The D-Bus call means "do not start the screensaver"; it is not user activity
and must not wake a screen that is already off.

## The patch
`0001-screen-off-inhibit-no-wake.patch`:
- cosmic-idle remembers that the screen is off (`fade_done`) until input resumes it.
- While it is off, a change of the screen-off timeout (an inhibitor taken or dropped, a config
  change) keeps the notification that turned the screen off. Replacing it would turn the outputs
  on.
- When input resumes the screen (`Resumed`, which the compositor's own input wake accompanies),
  the deferred timeout is applied. While a video still plays, the screen then stays on as before.
- An inhibitor taken during the fade, before the outputs are off, still cancels the fade.

## Build
```sh
ports/cosmic-idle/build.sh aarch64 x86_64
```
It applies the patch to the m6-session-bins tree with `patch(1)` (once; it is safe to rerun),
builds with the m6-session-bins toolchain, and stages `m6-session-bins/out/cosmic-idle-<arch>`,
which `scripts/mkfs-f2fs-populated.py` packs. `LEANDROS_COSMIC_IDLE=<file>` overrides it.
