# lane/deskplay — 2026-10-24 — laptop YouTube playback census (L3.1, L3.6, L3.7, L6.2, L11.3)

## Verdict: BLOCKED. Firefox never started playing, so the playback items could not be measured

| item | verdict | numbers |
|---|---|---|
| L3.1 TASKS during playback | NOT-MEASURED (no playback) | Firefox started but idle: 3.1-3.2k acq/s, 40 ms/s held, 143-155 cont/s, 2.1-2.3 ms/s spin. Page-load windows: 6.8-7.4k acq/s, 347-541 cont/s, 3.4-6.5 ms/s spin |
| L3.6 desktop CPU during playback | NOT-MEASURED | total 5 % idle-with-Firefox (cosmic-comp 0.6, cosmic-panel 0.6, brush 0.7, sh 1.2); page-load windows 15-20 % |
| L3.7 RDD PRIME export/close churn | NOT-MEASURED | RDD 0.0-0.2 %, close (nr3) not among its top syscalls, exec_vcs 0 (no decode) |
| L6.2 SSH blackout | OK while not playing; NOT-MEASURED under playback | 1 Hz persistent-SSH heartbeat over 694 s: max gap 2.2 s; ping 679/685 replies (6 single losses) |
| L11.3 Web Content spin after blank | NOT-MEASURED | needs a playing tab |

Kernel running: `6.0.0-leandros #1 i915ddb`, slot good = trial = `f3e3afc29d2a` (the good slot was changed before this lane;
laptopdesk measured on `8f46ed46696d`). Cmdline untouched. Laptop rebooted by another lane shortly before; session via
`scripts/desktop-login.sh` ("session up after ~6 s"). `blanktest --status` = On in every snapshot.

## What was run
- 4 launches of `su leandro -c "env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1 LIBVA_MESSAGING_LEVEL=0
  MOZ_AV_LOG_LEVEL=0 firefox https://www.youtube.com/watch?v=aqz-KE-bpKQ"` (profile user.js has media.autoplay.default=0,
  blocking_policy=0). Snapshots at +60, +90, +120 s after each launch. Tooling: `scripts/deskplay/` (dp.sh guest helper:
  launch/kill/snap/measure/blank; hb.sh Mac-side 1 Hz SSH heartbeat + ping; an.py window analyser over /proc/rqhold lock
  line, /proc/sysstat `t`/`s` lines, /proc/i915, wifi0 RX; phase1.sh driver).
- 4/4 launches: the same stall. First 30 s: firefox parent 6-13 %, wifi RX 23-130 KB/s; afterwards everything ~0 %,
  wifi RX 1 KB/s, Web Content 0.1-0.9 %, RDD 0-0.2 %, exec_vcs 0, compositor flips ~1/s (kms vblank log: 60 per minute).
- Probe after the 4th launch (after `blanktest --on`): firefox main thread in poll (sc 7), Compositor/Renderer/CanvasRenderer
  in futex wait, IPC I/O Parent / Socket Thread / WaylandProxy in epoll/poll: a normal idle Firefox. The Firefox log has
  only the gpu-env lines. This is the lane-ffhang signature (no frame callback, so Firefox treats its window as hidden
  and never issues the media requests), but with the panel On.

## Open / follow-ups
1. **New P1: Firefox started over SSH never paints in a `greetd-login` session on kernel f3e3afc29d2a.** Repro: reboot,
   `scripts/desktop-login.sh`, `sh scripts/deskplay/dp.sh launch` on the laptop, wait 60 s, `dp.sh snap x`: Web Content
   < 1 %, wifi RX ~1 KB/s, flips ~1/s. Two suspects to split: (a) the scripted greetd-IPC session (new since 2026-10-24;
   every earlier playback used a keyboard login): does cosmic-comp treat it as inactive or with the output off
   (1 Hz callbacks)?; (b) the i915ddb kernel in the good slot. Cheapest split: boot the previous good kernel
   (8f46ed46696d) once and repeat, or log in from the keyboard once on f3e3afc29d2a.
2. Then rerun `scripts/deskplay/phase1.sh` (3 pinned launches + 11-min heartbeat) and the blank/unblank step for L11.3.
