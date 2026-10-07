# lane/doomvis — DOOM invisible under COSMIC (2026-10-06)

User report: with the COSMIC desktop running, DOOM plays sound but shows no
picture, and no VT shows it either.

## Root cause

The shipped DOOM is the SDL build (`USE_SDL ?= 1` in Makefile.leandros):
`leandros_sdl.c` opens `/dev/dri/card0` and presents with the custom
0x1001/0x1002/0x1004 ioctls, with `/dev/fb0` as the fallback. Both target the
display hardware. Neither can be seen under a compositor:

1. **Launched from cosmic-term, DOOM is a KMS client on COSMIC's VT.** COSMIC
   is the DRM master there, so SET_MODE (0x1001) and FLIP_PAGE (0x1004) are
   refused EACCES by `master_gate` (drivers/src/drm_device_interface.rs,
   `requires_master`). CREATE_FB (0x1002) is not gated, so DOOM keeps
   rendering into a buffer nothing scans out. Sound uses a separate path, so
   it keeps playing. Linux behaves the same way: a raw KMS/fbdev program
   started from a terminal inside a desktop is invisible.
2. **No VT shows it, because its frames never targeted another VT.** A switch
   to a text VT repaints that VT's console. DOOM's buffer still belongs to
   COSMIC's VT, where it is refused.
3. **The fallbacks were broken too:**
   - `drm_create_framebuffer` refuses a buffer above 4 GiB (the legacy ABI
     returns a u32 physical address). The default 4 GiB guest hands one out,
     so CREATE_FB fails. DOOM then kept the *card0* fd as its "legacy FB" and
     `write()`d frames into the DRM node.
   - `/dev/fb0` writes (servers/vfs DevFb write) went into the console surface
     but only called `fb_flush`, which transfers only the console's own dirty
     rectangle. fbdev pixels never reached the host, and the console shadow's
     next blit overwrote them.
   - DRM master was a single device-wide slot. COSMIC drops master when its VT
     goes to the background (measured: `drop_ok` in /proc/vtstat). A KMS
     client on a text VT then took the slot implicitly, and on the switch back
     COSMIC's SET_MASTER would have been EBUSY for as long as that client
     lived.

## Fix

- **DOOM is a Wayland client when `WAYLAND_DISPLAY` is set**
  (`scripts/vendor/doomgeneric/leandros_wayland.h`, included by
  `leandros_sdl.c`). It speaks the wire protocol directly: wl_shm + xdg_shell
  + wl_seat keyboard, two 960x600 XRGB buffers (3x integer scale), and frame
  callbacks. There is no libwayland, because DOOM links the static LeandrOS
  libc. Keys come from `wl_keyboard`, whose codes are evdev codes, so DOOM
  only reads keys while its window has focus. Closing the window quits. When
  there is no display, DOOM falls back to KMS, then fb0.
- **libc `environ`** (userland/libc/src/start.rs) makes the environment
  reachable from C. leandros_wayland.h references it weakly, so an older libc
  still links.
- **DOOM's fallback opens /dev/fb0** rather than reusing the card0 fd.
- **fb0 writes reach the screen** (`fb_dev_written` in
  drivers/src/framebuffer.rs). The written rows are copied into the console
  shadow and transferred, but only while the console surface is on screen
  (text VT, no DRM scanout owner). Under a compositor it does nothing, as on
  Linux. A flush there would also re-point scanout 0 at the console.
- **One DRM master per VT** (`MASTER_BY_VT`). The active VT's slot is the only
  one that can present. Background holders are suspended and re-armed as
  before, and SET_MASTER/implicit grants take the active VT's slot. This is
  logind's "the foreground session is master" rule. /proc/vtstat gains a
  `master_by_vt` line. New vttest subtest `master_per_vt` (32/32).
- **Vendoring:** build-all.sh now syncs *every* file in
  scripts/vendor/doomgeneric/ into the shared sibling. Before, it synced only
  Makefile.leandros; `leandros_sdl.c` and `leandros_wayland.h` are now
  vendored too.

## Verification

`.claude/skills/run-leandros/doomsession.py <arch> <tag>` runs the whole
check. It does a greeter login, starts cosmic-term, and types
`doom -iwad /bin/doom1.wad` there. It photographs the window twice to show it
animates. It then switches to a text VT that is not COSMIC's, starts a second
DOOM from the serial root shell, switches back to COSMIC and to the text VT
again, and photographs and pixel-diffs every step.

| | aarch64 HVF virgl 4G | aarch64 HVF virgl 2G | x86_64 TCG virgl 4G (1920x1080) |
|---|---|---|---|
| window in COSMIC, animating | yes: 960x600 + border, 56 % of pixels change in 4 s | yes | yes, over cosmic-term; 34 % change between shots |
| DOOM on text VT 2 | yes, fullscreen via fb0 (CREATE_FB buffer >4 GiB) | yes, fullscreen via KMS | yes, fullscreen via KMS |
| back to COSMIC (VT 1) | desktop + DOOM window | desktop + window; `master_by_vt vt1=4 vt2=2`, `set_ebusy=0` | same as 2G aarch64 |
| text VT again | DOOM again | DOOM again | DOOM again |

In every run, /tmp/doom.log says `[doom] drawing in a Wayland window`. The
backgrounded KMS DOOM's presents are refused while its VT is off screen
(`refused_eacces` ~420), which is the intended suspension.

vttest: 32/32 on aarch64 and x86_64 (new `master_per_vt` passes on both).

## Left open

- CREATE_FB's 4 GiB limit (u32 legacy ABI) stays. Above it, DOOM uses fb0.
- When DOOM is a Wayland client, it renders at a fixed 3x scale and ignores
  configure sizes. Its min/max size hints ask the WM not to resize it.
- The F-keys of the Ctrl+Alt+Fn chord reach a raw-evdev DOOM on the text VT
  (F1 opens its help screen). That is expected for a program reading
  /dev/input/event0.
