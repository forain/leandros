# lane/blankplay — 2026-10-18

Goal (user): "Firefox shouldn't wake a blank screen and turn the display on if it was off from
inactivity or screen saver but still run and play videos with sound normally."

## Result

Three upstream COSMIC bugs, no kernel bug. All three are fixed as ports, and the laptop now
plays YouTube with sound, started over SSH after the idle blank, with the panel off throughout.

| | Before | After |
|---|---|---|
| Firefox started with the panel blanked | 0 % CPU, no requests (waits for its first frame callback) | loads and plays |
| A video starts while the panel is blank | panel turned back **on** for the whole video | panel stays off |
| Frame callbacks while an output is off | none | about 1/s per surface |
| An idle-inhibit-protocol inhibitor appears while the panel is blank | idle `resumed`, so cosmic-idle powers the panel on | ignored, because nothing on that output is visible |

## Root causes

### 1. cosmic-comp sends no frame callbacks while an output is powered off

`src/backend/kms/surface/mod.rs` (cosmic-comp `dec1ee86`; upstream master is the same):
- `set_dpms(false)` sends `DpmsOff`. The surface thread clears the CRTC and drops to `Idle`.
- `schedule_render()` is a no-op while `dpms` is false.
- Frame callbacks are only sent from that thread, after a real or an estimated vblank.

So with the output off, no client gets any frame callback. Firefox's `WaylandSurface` waits for
the first frame callback on its toplevel before it is "ready to draw". It has no fallback timer
for that one (its emulated-callback timer only runs once a buffer is attached), so a Firefox
started while the screen was blank never drew, and its startup never got as far as loading the
URL. That matches the lane-ffhang picture: every thread in a normal idle wait.

The kernel is not involved. With the output off nothing is submitted to KMS, so no page-flip or
vblank event can arrive, whatever the driver does. The same 0 callbacks show on virtio-gpu in
QEMU and on i915 on the laptop.

Fix: `ports/cosmic-comp/0002-kms-frame-callbacks-while-dpms-off.patch`. While the output is off, a
995 ms repeating timer on the surface thread advances the frame-callback sequence and sends frame
callbacks, with no render and no KMS commit. niri does the same for powered-off monitors. 995 ms
is cosmic-comp's own `THROTTLE` for invisible surfaces, and smithay throttles surfaces that are
not on their primary scan-out output to it anyway. The timer stops on DPMS on, `Resume` and
`Suspend`.

### 2. cosmic-idle turns the outputs on when a screensaver inhibitor appears

Found once fix 1 worked: Firefox played, but the panel came back on (`blanktest --status`: On;
i915 `flips` rising). Firefox takes `org.freedesktop.ScreenSaver.Inhibit` when a video plays
(`MOZ_LOG=LinuxWakeLock:5`: `switched to WakeLockType FreeDesktopScreensaver`). cosmic-idle
serves that interface. An inhibitor sets the screen-off time to `None`, and
`recreate_notifications()` replaces the idle notification and calls
`update_screen_off_idle(false)`, which sets every output On. Upstream master is the same.

Fix: `ports/cosmic-idle/0001-screen-off-inhibit-no-wake.patch`. cosmic-idle remembers that it
turned the screen off (`fade_done`). While the screen is off, a change of the timeout keeps the
notification that turned it off. Input resumes the screen, and the compositor's own input wake
comes with that; `Resumed` then applies the deferred timeout. While a video still plays, the
screen then stays on as before. An inhibitor taken during the fade, before the outputs are off,
still cancels the fade.

Firefox flips its wakelock between `locked-foreground` and `locked-background` many times while
the window is not presented: 53 inhibit requests in 13 min. Each one used to wake the screen.

### 3. An idle-inhibit-protocol inhibitor resumes idle, which wakes the screen

cosmic-comp's `IdleInhibitHandler::inhibit` called smithay's `set_is_inhibited(true)` for every
new `zwp_idle_inhibitor_v1`. That sends `resumed` to every idle notification that is idle, and
cosmic-idle answers `resumed` by powering the outputs on. The protocol only inhibits while the
surface is visible. `refresh_idle_inhibit()` checks that through the primary scan-out output,
but `inhibit()` skipped it, and that output is stale while the output is off. Firefox uses D-Bus,
not this path, but mpv and similar players use it.

Fix: `ports/cosmic-comp/0003-idle-inhibit-ignores-powered-off-outputs.patch`.
`Common::powered_off_outputs` holds the outputs turned off through output power management;
input clears it. `refresh_idle_inhibit()` ignores surfaces on those outputs, and `inhibit()` goes
through it.

## Tests

- `userland/blanktest` (`/bin/blanktest`) is a std/musl Wayland client using the pure-Rust
  wayland-client. It runs four checks:
  - Window A animates on frame callbacks with the outputs on.
  - Every output is powered off through `zwlr_output_power_v1`. A must get at least 3 callbacks
    in 6 s.
  - Window B is mapped while the outputs are off. It must get its first callback within 5 s.
  - An `ext_idle_notification_v1` (1 s) goes idle. An idle inhibitor on window A must not resume
    it.
  - No output may report mode On while the test holds the outputs off.

  Afterwards it turns the outputs back on. `--off`, `--on` and `--status` only switch the outputs
  or report them; `--off` is a way to blank a screen remotely.
- `.claude/skills/run-leandros/blanktest.py <arch>` boots a GPU session with `ffsession.py
  --nofirefox` and runs it. It cannot go in `regress`: that suite boots headless, with no
  compositor.

| Where | cosmic-comp | blanktest |
|---|---|---|
| QEMU aarch64 HVF virgl | main | FAIL: A 0 callbacks in 6 s off, B no callback |
| QEMU aarch64 HVF virgl | + 0002 | PASS: A 6 callbacks in 6 s off, B first callback after 0.47 s, never woken |
| laptop i915, greeter comp | main | FAIL: same 0 / none |
| laptop i915, session comp | + 0002 | PASS: 6 in 6 s, B after 0.46 s (before the inhibit check existed) |
| laptop i915, session comp | + 0002 | FAIL on the inhibit check only: `resumed 1` |
| laptop i915, greeter comp | + 0002 + 0003 | PASS: 6 in 6 s, B after 0.47 s, `resumed 0` |

QEMU, patched comp, outputs off (`blanktest --off`): a Firefox started on YouTube pulled 13 MB
in 40 s. In the same image with the screen on it behaves the same. YouTube video does not play
in QEMU on this Mac in either state, so playback is checked on the laptop only.

## Laptop evidence (i915, good kernel ca453c25ce01 = main b6e6f363)

The session ran patched cosmic-comp + cosmic-idle (pushed to `/bin`, originals kept as
`/bin/*.main`). It was started through greetd IPC. After about 15 min without input, cosmic-idle
blanked the panel by itself (`blanktest --status`: Off, i915 `flips` 19904). Then
`su leandro -c "firefox https://www.youtube.com/watch?v=aqz-KE-bpKQ"` was run over SSH:

| Window | Web Content | firefox | RDD | pipewire-pulse | snd-sink | wifi0 RX | panel |
|---|---|---|---|---|---|---|---|
| 0-30 s | 77 % | 60 % | 27 % | 3.3 % | — | +11.1 MiB | Off |
| 30-60 s | 60 % | 43 % | 6.7 % | 3.3 % | 3.3 % | +4.8 MiB | Off |
| 60 s window | — | 33 % | — | 3.3 % | 3.3 % | +4.6 MiB/min | Off |
| t ≈ 13 min | 43 % | 33 % | 10 % | 3.3 % | 3.3 % | +2.2 MiB/30 s | Off, `flips` still 19904 |

So Firefox played with sound, with the panel off, and no page flip happened in 13 min. RDD
falls after the first 30 s. That is most likely Firefox suspending video decode for a window it
treats as hidden: its wakelock flips between `locked-foreground` and `locked-background`. Audio
keeps playing.

Same session, before the cosmic-idle fix (comp fix only): Firefox played, but the panel was lit
again within 40 s (`blanktest --status` On, i915 `flips` 5165 → rising).

Final run, all three patches. The session was started 13:30 and blanked by idle at 13:46
(`flips` 21379). Firefox on the same video was started at 13:46:30 over SSH:
- 0-40 s: firefox 35 %, Web Content 28 %, RDD 7.5 %, pipewire-pulse 2.5 %, wifi0 +6.9 MiB.
- Next 60 s: leandros-snd-sink +2 s and pipewire-pulse +2 s of CPU (3.3 % each), firefox 50 %,
  wifi0 +10.1 MiB.
- Afterwards: `blanktest --status` reported Off, `flips` was still 21379, and cosmic-comp ran at
  about 3 %.

After the runs, Firefox was killed and `/bin/cosmic-comp` and `/bin/cosmic-idle` were restored
to main's binaries (sha256 checked against `m6-session-bins/out`). The session was ended, so the
greeter runs main's cosmic-comp. The laptop is on good ca453c25ce01, default good, not armed.
No kernel was deployed.

## Web Content spin (TODO "Web Content spin on the epoll.ready=0 boot")

It was not seen in this session. With the panel off and frame callbacks at about 1/s, Web
Content ran at 43-77 % during playback, which is the same range as with the screen lit
(lane-ffhang: 53-113 %). The original 2.8-3.4-core spin was 4.4 h into a boot with **no**
callbacks at all, so it fits fix 1, but it is not reproduced and stays unverified.

## Open

- Frame callbacks while off are about 1/s, not the refresh rate. That is enough for Firefox to
  start, load and play audio. A client that renders continuously, such as a game, runs at 1 fps
  while the screen is off, which is the point.
- **Integration**: after merge, run `ports/cosmic-comp/build.sh aarch64 x86_64` (0002 and 0003) and
  `ports/cosmic-idle/build.sh aarch64 x86_64`. They patch the shared m6-session-bins trees and
  stage `out/`. This lane did not patch those trees or `out/`: it built from patched scratch
  copies with `CARGO_TARGET_DIR` set to the shared trees' `target/` (to reuse the dependencies),
  so `target/<triple>/release/cosmic-{comp,idle}` there currently hold this lane's builds.
  `build.sh` deletes and rebuilds them. Then rebuild the images and put the new `/bin/cosmic-comp` and `/bin/cosmic-idle` on
  the stick.
