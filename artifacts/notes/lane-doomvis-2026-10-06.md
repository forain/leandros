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

## Retest 2026-10-07: "no window from cosmic-term, no sound anywhere"

The user built lane/doomvis in this worktree and still got no window, DOOM on
VT 2 without keyboard, and no sound in any scenario. Three root causes:

1. **The image shipped another tree's DOOM.** `doom-<arch>` lived at a shared
   path in `../doomgeneric`. The main checkout relinked it at 00:20 against
   main's libc (no `environ`; `nm` showed `w environ`, an unresolved weak
   reference). This worktree's objects were older than that binary, so make
   called it up to date and mkfs packed it. Its `environ` was NULL, so DOOM saw
   no WAYLAND_DISPLAY and silently went the KMS way:
   - The per-VT master refused it on COSMIC's VT.
   - It was granted VT 2 as soon as the user switched there.
   - Its evdev queue stayed pinned to VT 1, which explains the dead keyboard.

   Fix: build_doom links into its OBJDIR and copies to `target/doom/`, which
   mkfs prefers. The libc is a link prerequisite. When `environ` is missing,
   DOOM reads the envp block after argv.
2. **No sound since 24bf6e8 (2026-10-02), not this branch.** The audio device
   is single-writer, and PipeWire's sink holds it for the whole desktop
   session. DOOM's legacy IPC got EBUSY, and DOOM ignored it.
   - Fix: DOOM plays through the session's PipeWire. It writes a streaming WAV
     into `pw-cat` (this pw-cat has no raw mode), and reports refusals.
   - Text VT: console logins had no XDG_RUNTIME_DIR, so DOOM could not find the
     user's PipeWire. login now sets `/run/user/<uid>`, as pam_systemd does.
3. **Silent fallback to KMS under a compositor.** DOOM now prints the reason
   whenever Wayland is not used. It exits if a configured Wayland display is
   unusable, or if KMS SET_MODE is refused with EACCES. It never takes over
   another VT.

aarch64 user-flow run (`doomsession.py aarch64 u-a64c`):
- **Desktop:** DOOM opened a Wayland window and played through PipeWire (RMS
  2511, 11 of 12 s audible). Esc opened the menu and Enter x3 started E1M1.
- **After quitting the desktop DOOM:** silence (RMS 0).
- **Text VT 2, console login as leandro, `doom`:** KMS display, PipeWire sound
  (RMS 2531), and Esc opened the menu.
- **Back on VT 1:** the desktop presents again.

x86_64 TCG user-flow run (`doomsession.py x86_64 u-x64`):
- Wayland window and PipeWire sound in the desktop. Esc opens the menu and
  Enter x3 starts a game.
- Silence after quitting the desktop DOOM.
- Text VT 2 (console login as leandro): KMS display, PipeWire sound, Esc
  opens the menu.

No VT takeover (aarch64, `novtsteal` probe):
- In cosmic-term, `WAYLAND_DISPLAY=bogus doom` prints `cannot connect ...
  errno 2` and `not falling back to the display hardware ...`, then exits.
- From the serial root shell with COSMIC on screen, `doom` prints `the display
  is owned by another program (DRM master, EACCES)` and exits.
- No DOOM process is left, and after Ctrl+Alt+F2 `master_by_vt` is all zero
  (no grant on VT 2).

**Open, pre-existing: x86_64 DOOM audio clips.** The loudest 10 s have 32 %
of samples at full scale through PipeWire, and 39 % on the direct device with
no PipeWire running. aarch64 peaks at 16447 with no clipping. The fault is in
DOOM's own mixing/MIDI output on x86_64, not in either output path.
